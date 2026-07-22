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

async function installMutableVisualViewport(
  page: Page,
  metrics: { layoutHeight: number; visualHeight: number; offsetTop: number },
): Promise<void> {
  await page.addInitScript(({ layoutHeight, visualHeight, offsetTop }) => {
    const scope = globalThis as typeof globalThis & {
      __TERMD_TEST_SET_VISUAL_VIEWPORT__?: (next: { layoutHeight: number; visualHeight: number; offsetTop: number }) => void;
    };
    const target = new EventTarget();
    const state = {
      layoutHeight,
      visualHeight,
      offsetTop,
    };
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      get: () => state.layoutHeight,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: {
        get height() {
          return state.visualHeight;
        },
        get width() {
          return window.innerWidth;
        },
        get offsetTop() {
          return state.offsetTop;
        },
        get offsetLeft() {
          return 0;
        },
        get pageTop() {
          return state.offsetTop;
        },
        get pageLeft() {
          return 0;
        },
        get scale() {
          return 1;
        },
        addEventListener: target.addEventListener.bind(target),
        removeEventListener: target.removeEventListener.bind(target),
        dispatchEvent: target.dispatchEvent.bind(target),
      },
      writable: true,
    });
    scope.__TERMD_TEST_SET_VISUAL_VIEWPORT__ = (next) => {
      state.layoutHeight = next.layoutHeight;
      state.visualHeight = next.visualHeight;
      state.offsetTop = next.offsetTop;
      target.dispatchEvent(new Event("resize"));
      window.dispatchEvent(new Event("resize"));
    };
  }, metrics);
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

test.beforeEach(async ({ page }) => {
  await resetBrowserState(page);
});

test("mobile visibility resume replaces a half-open terminal connection once", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "该回归只需要移动端项目覆盖");
  test.setTimeout(60_000);
  const sessionId = "00000000-0000-0000-0000-0000000005f0";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "mobile-resume-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "mobile-resume-ready", 8_000);
    await expect.poll(() => daemon.attachedSessions).toEqual([sessionId]);

    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      Object.defineProperty(document, "hidden", {
        configurable: true,
        get: () => true,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    daemon.suspendTerminalConnectionsWithoutClose();
    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      Object.defineProperty(document, "hidden", {
        configurable: true,
        get: () => false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("focus"));
    });

    await expect.poll(() => daemon.attachedSessions, { timeout: 8_000 }).toEqual([sessionId, sessionId]);
    await page.waitForTimeout(300);
    expect(daemon.attachedSessions).toEqual([sessionId, sessionId]);
    daemon.pushSessionData(sessionId, "mobile-resume-output\n");
    await expectTerminalLine(page, "mobile-resume-output", 8_000);
  } finally {
    await page.evaluate(() => {
      Reflect.deleteProperty(document, "visibilityState");
      Reflect.deleteProperty(document, "hidden");
    }).catch(() => undefined);
    await daemon.stop();
  }
});

test("mobile terminal pointerdown 提前解锁 focus suppression，helper textarea 不会被立即 blur", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "该回归只需要移动端项目覆盖");
  test.setTimeout(60_000);

  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-0000000005f1",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "mobile-focus-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "mobile-focus-ready", 8_000);

    const terminalSurface = page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first();
    const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();

    await terminalSurface.tap({ position: { x: 20, y: 20 } });
    await expect(terminalInput).toBeFocused({ timeout: 10_000 });

    await terminalInput.evaluate((element) => {
      (element as HTMLTextAreaElement).blur();
    });
    await page.waitForTimeout(180);
    const resizeCountBeforeBypassFocus = daemon.sessionResizes.length;

    await terminalSurface.dispatchEvent("pointerdown", {
      pointerId: 91,
      pointerType: "touch",
      button: 0,
      clientX: 24,
      clientY: 24,
    });

    await expect(terminalInput).toBeFocused({ timeout: 10_000 });
    await expect.poll(() => daemon.sessionResizes.length, { timeout: 10_000 }).toBe(resizeCountBeforeBypassFocus);
    const inputBaseline = daemon.decryptedInputs.join("");
    await terminalInput.evaluate((element) => {
      element.dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: "x",
        }),
      );
    });
    await expect.poll(() => daemon.decryptedInputs.join("").slice(inputBaseline.length), { timeout: 10_000 }).toBe("x");
  } finally {
    await daemon.stop();
  }
});

test("mobile terminal 迟到 contextmenu 不会收起 helper textarea 焦点", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "该回归只需要移动端项目覆盖");
  await enableTermdDiagnostics(page);

  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-0000000005f3",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "mobile-contextmenu-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "mobile-contextmenu-ready", 8_000);

    const terminalSurface = page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first();
    const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();

    // 中文注释：当前移动端语义已经改成“明确 tap/click 才激活输入”，不能再用
    // pointerdown 充当键盘唤起边界。这里先建立一次真实 tap，再验证迟到 contextmenu
    // 不会把已经存在的 helper textarea 焦点收走。
    await terminalSurface.tap({ position: { x: 24, y: 24 } });
    await expect(terminalInput).toBeFocused();

    await page.waitForTimeout(800);
    await terminalSurface.dispatchEvent("contextmenu", {
      bubbles: true,
      cancelable: true,
      clientX: 24,
      clientY: 24,
    });
    await expect(terminalInput).toBeFocused();

    const inputBaseline = daemon.decryptedInputs.join("");
    await terminalInput.evaluate((element) => {
      // 中文注释：焦点还在 helper textarea 后，移动端 beforeinput 兜底仍应能把文本送进 PTY。
      element.dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: "c",
        }),
      );
    });
    await expect.poll(() => daemon.decryptedInputs.join("").slice(inputBaseline.length)).toBe("c");
  } finally {
    await attachTermdDiagnostics(testInfo, "mobile-contextmenu", page);
    await daemon.stop();
  }
});

test("mobile keyboard open 会重排 shell 高度且不写回 PTY", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "该回归只需要移动端项目覆盖");

  await installMutableVisualViewport(page, { layoutHeight: 820, visualHeight: 820, offsetTop: 0 });
  const sessionId = "00000000-0000-0000-0000-0000000005f2";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: `${Array.from({ length: 48 }, (_, index) => `mobile-keyboard-line-${String(index + 1).padStart(2, "0")}`).join("\n")}\n`,
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "mobile-keyboard-line-48", 8_000);

    const shell = page.locator(".app-shell");
    const terminalSurface = page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first();
    const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();
    await terminalSurface.click({ position: { x: 20, y: 20 } });
    await expect(terminalInput).toBeFocused();
    const inputBaseline = daemon.decryptedInputs.join("");
    await terminalInput.evaluate((element) => {
      // 中文注释：真实移动浏览器打开软键盘时，visualViewport 变矮常常慢于
      // focusout/window blur；这里先让 helper textarea 短暂掉到 body。
      (element as HTMLTextAreaElement).blur();
    });
    await page.evaluate(() => window.dispatchEvent(new Event("blur")));
    await expect(terminalInput).toBeFocused();
    await terminalInput.evaluate((element) => {
      element.dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: "k",
        }),
      );
    });
    await expect.poll(() => daemon.decryptedInputs.join("").slice(inputBaseline.length)).toBe("k");

    const resizeCountBeforeKeyboardOpen = daemon.sessionResizes.length;
    await page.evaluate(() => {
      (window as typeof window & {
        __TERMD_TEST_SET_VISUAL_VIEWPORT__?: (next: { layoutHeight: number; visualHeight: number; offsetTop: number }) => void;
      }).__TERMD_TEST_SET_VISUAL_VIEWPORT__?.({
        layoutHeight: 820,
        visualHeight: 460,
        offsetTop: 20,
      });
    });

    await expect(shell).toHaveClass(/mobile-keyboard-open/);
    await expect.poll(async () =>
      shell.evaluate((element) => (element as HTMLElement).style.getPropertyValue("--termd-layout-viewport-height")),
    ).toBe("460px");
    await expect.poll(async () =>
      shell.evaluate((element) => (element as HTMLElement).style.getPropertyValue("--termd-visual-viewport-height")),
    ).toBe("460px");
    await expect(terminalInput).toBeFocused();
    // 中文注释：TerminalPane 的 resize 上报有多帧稳定窗口；负向断言必须
    // 等过这段窗口，避免测试在延迟 resize 发出前提前通过。
    await page.waitForTimeout(900);
    expect(daemon.sessionResizes.length).toBe(resizeCountBeforeKeyboardOpen);

    const terminalRows = await page.locator(".terminal-host").evaluate((host) =>
      Number((host as HTMLElement).dataset.termdActualRows),
    );
    expect(terminalRows).toBeGreaterThan(0);
    const centerCursorRow = Math.floor(terminalRows / 2);
    // 中文注释：先把 PTY 光标移到网格中部，验证未触及上下边界时仍会居中。
    daemon.pushSessionData(sessionId, `\u001b[${centerCursorRow + 1};1Hmobile-keyboard-cursor-anchor`);
    await expectTerminalLine(page, "mobile-keyboard-cursor-anchor", 8_000);
    const readMobileCursorGeometry = (cursorRow: number) => page.locator(".terminal-scrollport").evaluate(
      (element, expectedCursorRow) => {
        const scrollport = element as HTMLElement;
        const pane = scrollport.closest<HTMLElement>(".terminal-pane");
        const toolbar = document.querySelector<HTMLElement>(".toolbar");
        const shortcuts = pane?.querySelector<HTMLElement>(".terminal-mobile-shortcuts");
        const host = pane?.querySelector<HTMLElement>(".terminal-host");
        const screen = host?.querySelector<HTMLElement>(".xterm-screen");
        const input = host?.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        if (!pane || !toolbar || !shortcuts || !host || !screen || !input) {
          return undefined;
        }
        const scrollportRect = scrollport.getBoundingClientRect();
        const paneRect = pane.getBoundingClientRect();
        const toolbarRect = toolbar.getBoundingClientRect();
        const shortcutsRect = shortcuts.getBoundingClientRect();
        const screenRect = screen.getBoundingClientRect();
        const inputRect = input.getBoundingClientRect();
        const terminalRows = Number(host.dataset.termdActualRows);
        const viewportLinesAboveBottom = Number(host.dataset.termdViewportYRaw);
        if (!terminalRows || !Number.isFinite(viewportLinesAboveBottom) || screenRect.height <= 0) {
          return undefined;
        }
        const rowHeight = screenRect.height / terminalRows;
        const cursorViewportRow = expectedCursorRow + viewportLinesAboveBottom;
        const cursorCenter = screenRect.top + (cursorViewportRow + 0.5) * rowHeight;
        const visibleCenter = (scrollportRect.top + scrollportRect.bottom) / 2;
        const visualBottom = (window.visualViewport?.offsetTop ?? 0) + (window.visualViewport?.height ?? window.innerHeight);
        return {
          paneStartsAtToolbar: paneRect.top >= toolbarRect.bottom - 1,
          paneEndsAtKeyboard: Math.abs(paneRect.bottom - visualBottom) <= 2,
          terminalEndsAtShortcuts: Math.abs(scrollportRect.bottom - shortcutsRect.top) <= 2,
          cursorVisible: cursorCenter >= scrollportRect.top && cursorCenter <= scrollportRect.bottom,
          cursorCentered: Math.abs(cursorCenter - visibleCenter) <= 2,
          screenStartsAtTerminalTop: Math.abs(screenRect.top - scrollportRect.top) <= 2,
          screenEndsAtTerminalBottom: Math.abs(screenRect.bottom - scrollportRect.bottom) <= 2,
          inputAnchorAligned: Math.abs(inputRect.top - (cursorCenter - rowHeight / 2)) <= 2,
        };
      },
      cursorRow,
    );
    const expectedMobileCursorGeometry = {
      paneStartsAtToolbar: true,
      paneEndsAtKeyboard: true,
      terminalEndsAtShortcuts: true,
      cursorVisible: true,
      inputAnchorAligned: true,
    };
    await expect.poll(() => readMobileCursorGeometry(centerCursorRow)).toMatchObject({
      ...expectedMobileCursorGeometry,
      cursorCentered: true,
    });

    // 中文注释：移动浏览器可能在原生键盘输入时滚动 focused textarea 的祖先。
    // daemon 故意不回显，确保光标与 IME 锚点无需等待 PTY 输出也能恢复居中。
    const noEchoInputBaseline = daemon.decryptedInputs.join("");
    await page.keyboard.type("x");
    await expect.poll(() => daemon.decryptedInputs.join("").slice(noEchoInputBaseline.length)).toBe("x");
    await expect.poll(() => readMobileCursorGeometry(centerCursorRow)).toMatchObject({
      ...expectedMobileCursorGeometry,
      cursorCentered: true,
    });

    daemon.pushSessionData(sessionId, "\u001b[1;1Hmobile-keyboard-top-anchor");
    await expectTerminalLine(page, "mobile-keyboard-top-anchor", 8_000);
    await expect.poll(() => readMobileCursorGeometry(0)).toMatchObject({
      ...expectedMobileCursorGeometry,
      screenStartsAtTerminalTop: true,
    });

    // 中文注释：首行贴住标题区后，点击终端顶部仍应从工具栏拿回 helper textarea 焦点。
    const mobileMenuToggle = page.locator(".mobile-menu-toggle");
    await mobileMenuToggle.focus();
    await expect(mobileMenuToggle).toBeFocused();
    const scrollportBox = await page.locator(".terminal-scrollport").boundingBox();
    if (!scrollportBox) {
      throw new Error("mobile terminal scrollport has no layout box");
    }
    await page.touchscreen.tap(scrollportBox.x + 16, scrollportBox.y + 8);
    await expect(terminalInput).toBeFocused();

    daemon.pushSessionData(sessionId, `\u001b[${terminalRows};1Hmobile-keyboard-bottom-anchor`);
    await expectTerminalLine(page, "mobile-keyboard-bottom-anchor", 8_000);
    await expect.poll(() => readMobileCursorGeometry(terminalRows - 1)).toMatchObject({
      ...expectedMobileCursorGeometry,
      screenEndsAtTerminalBottom: true,
    });

    // 中文注释：末行贴住键盘上方后，触摸拖动仍必须由终端滚动手势消费，
    // 不能让外层 scrollport 自己漂移。
    const terminalDrag = await page.locator(".terminal-scrollport").evaluate((element) => {
      const scrollport = element as HTMLElement;
      const canvas = scrollport.querySelector<HTMLElement>(".terminal-canvas");
      if (!canvas) {
        return undefined;
      }
      const rect = scrollport.getBoundingClientRect();
      const clientX = rect.left + 20;
      const startY = rect.bottom - 100;
      const endY = rect.bottom - 20;
      const scrollTopBefore = scrollport.scrollTop;
      canvas.dispatchEvent(new PointerEvent("pointerdown", {
        bubbles: true,
        cancelable: true,
        pointerId: 91,
        pointerType: "touch",
        buttons: 1,
        clientX,
        clientY: startY,
      }));
      const moveAccepted = canvas.dispatchEvent(new PointerEvent("pointermove", {
        bubbles: true,
        cancelable: true,
        pointerId: 91,
        pointerType: "touch",
        buttons: 1,
        clientX,
        clientY: endY,
      }));
      canvas.dispatchEvent(new PointerEvent("pointerup", {
        bubbles: true,
        cancelable: true,
        pointerId: 91,
        pointerType: "touch",
        buttons: 0,
        clientX,
        clientY: endY,
      }));
      return {
        movePrevented: !moveAccepted,
        scrollTopBefore,
        scrollTopAfter: scrollport.scrollTop,
      };
    });
    expect(terminalDrag).toBeDefined();
    expect(terminalDrag?.movePrevented).toBe(true);
    expect(terminalDrag?.scrollTopAfter).toBe(terminalDrag?.scrollTopBefore);
    await expect.poll(() => page.locator(".terminal-host").evaluate((host) =>
      Number((host as HTMLElement).dataset.termdViewportYRaw),
    )).toBeGreaterThan(0);

    const restoreBottomInputBaseline = daemon.decryptedInputs.join("");
    await page.keyboard.type("z");
    await expect.poll(() => daemon.decryptedInputs.join("").slice(restoreBottomInputBaseline.length)).toBe("z");
    await expect.poll(() => readMobileCursorGeometry(terminalRows - 1)).toMatchObject({
      ...expectedMobileCursorGeometry,
      screenEndsAtTerminalBottom: true,
    });
    expect(daemon.sessionResizes.length).toBe(resizeCountBeforeKeyboardOpen);

    await page.evaluate(() => {
      (window as typeof window & {
        __TERMD_TEST_SET_VISUAL_VIEWPORT__?: (next: { layoutHeight: number; visualHeight: number; offsetTop: number }) => void;
      }).__TERMD_TEST_SET_VISUAL_VIEWPORT__?.({
        layoutHeight: 820,
        visualHeight: 820,
        offsetTop: 0,
      });
    });
    await expect(shell).not.toHaveClass(/mobile-keyboard-open/);
    await expect.poll(() => page.locator(".terminal-scrollport").evaluate((element) => {
      const scrollport = element as HTMLElement;
      const frame = scrollport.querySelector<HTMLElement>(".terminal-frame");
      const canvas = scrollport.querySelector<HTMLElement>(".terminal-canvas");
      const host = scrollport.querySelector<HTMLElement>(".terminal-host");
      return {
        scrollTop: scrollport.scrollTop,
        frameTop: frame?.style.top ?? "missing",
        frameHeight: frame?.style.height ?? "missing",
        canvasHeight: canvas?.style.height ?? "missing",
        viewportLinesAboveBottom: Number(host?.dataset.termdViewportYRaw),
      };
    })).toEqual({
      scrollTop: 0,
      frameTop: "",
      frameHeight: "",
      canvasHeight: "",
      viewportLinesAboveBottom: 0,
    });
  } finally {
    await daemon.stop();
  }
});

test("pair、list、attach 的浏览器 smoke", async ({ page }, testInfo: TestInfo) => {
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-000000000501",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        activity: { kind: "ai", agent: "codex", state: "running", changed_at_ms: 10 },
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
    await expect(page.getByLabel("Pairing token")).toHaveValue(/termd-pair:v2:/);
    await activateButton(page, "Pair");

    await expect(page.getByLabel("Pairing token")).toBeHidden();
    const terminalPane = page.getByTestId("terminal-pane");
    await expectTerminalLine(page, "termd-e2e-ready", 8_000);

    const openProgressButton = terminalPane.getByRole("button", { name: "Session opening progress" });
    await expect(openProgressButton).toBeVisible();
    const openProgressIcon = openProgressButton.locator("svg[data-open-progress-icon='ready']");
    await expect(openProgressIcon).toBeVisible();
    await expect(openProgressIcon).toHaveCSS("width", "18px");
    await expect(openProgressIcon).toHaveCSS("height", "18px");
    await expect(openProgressButton.locator("img[data-session-avatar]")).toHaveCount(0);

    await openProgressButton.click();
    const openProgressPopover = terminalPane.getByTestId("terminal-open-progress");
    await expect(openProgressPopover).toBeVisible();
    const progressButtonBox = await openProgressButton.boundingBox();
    const progressPopoverBox = await openProgressPopover.boundingBox();
    expect(progressButtonBox).not.toBeNull();
    expect(progressPopoverBox).not.toBeNull();
    expect(progressPopoverBox!.y).toBeGreaterThanOrEqual(progressButtonBox!.y + progressButtonBox!.height + 4);
    if (testInfo.project.name === "chromium") {
      await page.setViewportSize({ width: 820, height: 768 });
      await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
      await expect(page.getByRole("button", { name: "Show files panel" })).toBeVisible();
      await expect.poll(() => terminalPane.evaluate((pane) => {
        const popover = pane.querySelector<HTMLElement>('[data-testid="terminal-open-progress"]');
        if (!popover) return false;
        const terminalRect = pane.getBoundingClientRect();
        const popoverRect = popover.getBoundingClientRect();
        return popoverRect.left >= terminalRect.left && popoverRect.right <= terminalRect.right;
      })).toBe(true);
      await page.screenshot({ path: "test-results/session-open-progress-narrow-desktop.png", fullPage: true });
      await page.setViewportSize({ width: 1366, height: 768 });
    }
    await openProgressButton.click();
    await expect(openProgressPopover).toBeHidden();
    if (testInfo.project.name === "chromium") {
      // 窄桌面收起状态会在宽屏保留；后续断言需要显式恢复会话栏。
      await page.getByRole("button", { name: "Expand sidebar" }).click();
      await expect(page.getByRole("region", { name: "sessions" })).toBeVisible();
      // 文件面板同样保留收起状态；后续会验证其 resizer，先按用户路径打开。
      await page.getByRole("button", { name: "Show files panel" }).click();
      await expect(page.getByRole("separator", { name: "Resize files panel" })).toBeVisible();
    }

    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toHaveCount(0);
      const sessionListRequests = () =>
        daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list").length;
      const beforeTitlePull = sessionListRequests();
      const beforeMetadataConnections = daemon.v070MetadataConnections;
      const titleButton = page.getByRole("button", { name: "Open session list from title" });
      // 中文注释：0.7 session list 由固定 metadata WebSocket 实时推送；移动端
      // 标题下拉只重读已有 snapshot，不能额外创建连接或发起旧 session.list RPC。
      await titleButton.dispatchEvent("pointerdown", {
        pointerId: 31,
        pointerType: "touch",
        button: 0,
        clientX: 180,
        clientY: 18,
      });
      await titleButton.dispatchEvent("pointermove", {
        pointerId: 31,
        pointerType: "touch",
        buttons: 1,
        clientX: 182,
        clientY: 82,
      });
      await titleButton.dispatchEvent("pointerup", {
        pointerId: 31,
        pointerType: "touch",
        button: 0,
        clientX: 182,
        clientY: 82,
      });
      await page.waitForTimeout(250);
      expect(sessionListRequests()).toBe(beforeTitlePull);
      expect(daemon.v070MetadataConnections).toBe(beforeMetadataConnections);
      await expect(page.getByRole("dialog", { name: "sessions panel" })).toBeHidden();

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
      await expect(page.getByRole("dialog", { name: "sessions panel" })).toBeVisible();
      await activateButton(page, "Refresh sessions");

      // 回归断言：移动端顶部入口必须留在左侧，避免刷新后被布局规则顶到右边。
      const mobileMenuButton = page.getByRole("button", { name: "Open mobile workspace menu" });
      const menuBox = await mobileMenuButton.boundingBox();
      expect(menuBox?.x ?? 0).toBeLessThan(48);
    } else {
      const daemonStatus = page.getByRole("contentinfo", { name: "daemon server status" });
      await expect(daemonStatus).toBeVisible();
      await expect(daemonStatus.getByText("CPU", { exact: true })).toBeVisible();
      await expect(daemonStatus.getByRole("button", { name: "Refresh server status" })).toHaveCount(0);
    }

    if (testInfo.project.name !== "mobile-chrome") {
      const terminalHost = terminalPane.locator(".terminal-host[role='textbox']");
      const terminalTextarea = terminalPane.locator('textarea[aria-label="Terminal input"]');
      await terminalPane.locator(".terminal-host .xterm-screen, .terminal-host canvas").first().click({ position: { x: 20, y: 20 } });
      await expect(terminalTextarea).toBeFocused();
      await expect(terminalHost).not.toBeFocused();
      const compositionBaseline = daemon.decryptedInputs.join("");
      await terminalTextarea.evaluate((element) => {
        element.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true, cancelable: true }));
        element.dispatchEvent(
          new KeyboardEvent("keydown", {
            bubbles: true,
            cancelable: true,
            code: "Space",
            key: " ",
            isComposing: true,
          }),
        );
      });
      await expect.poll(() => daemon.decryptedInputs.join("")).toBe(compositionBaseline);
      await terminalTextarea.evaluate((element) => {
        const input = element as HTMLTextAreaElement;
        input.value = "，";
        element.dispatchEvent(
          new CompositionEvent("compositionend", {
            bubbles: true,
            cancelable: true,
            data: "，",
          }),
        );
      });
      await expect.poll(() => daemon.decryptedInputs.join("").slice(compositionBaseline.length)).toBe("，");
      await page.keyboard.type("desktop-focus-ok");
      await page.keyboard.press("Enter");
      await expect.poll(() => daemon.decryptedInputs.join("")).toContain("desktop-focus-ok");
    }

    const sessionsPanel = page.getByRole("region", { name: "sessions" });
    // session UUID 已从 UI 隐藏；测试按用户实际看到的可访问名称打开会话。
    const openSessionButton = sessionsPanel.getByRole("button", { name: "Open Lagrange, Codex is running" });
    const sessionRow = sessionsPanel.locator(".session-row").filter({
      has: page.getByRole("button", { name: "Open Lagrange, Codex is running" }),
    });
    await expect(sessionRow).toBeVisible();
    const sessionAvatarFrame = sessionRow.locator(".session-avatar");
    const sessionAvatar = sessionRow.locator("img[data-avatar-style='identicon']");
    await expect(sessionAvatar).toBeVisible();
    await expect(sessionAvatarFrame).toHaveCSS("width", "26px");
    await expect(sessionAvatarFrame).toHaveCSS("height", "26px");
    await expect.poll(() => sessionAvatar.evaluate((image: HTMLImageElement) => image.naturalWidth)).toBeGreaterThan(0);
    const sessionVisual = sessionRow.getByTitle("Codex is running");
    await expect(sessionVisual).toHaveAttribute("title", "Codex is running");
    await expect(sessionVisual).toHaveAttribute("aria-hidden", "true");
    await expect(sessionVisual).toHaveClass(/activity-running/);
    await expect(sessionVisual).toHaveCSS("width", "26px");
    await expect(sessionVisual).toHaveCSS("height", "26px");
    await expect(sessionVisual.locator("svg")).toHaveCount(0);
    const sessionActivityIndicator = sessionRow.locator(".session-activity-indicator");
    await expect(sessionActivityIndicator).toHaveText("Codex is running");
    await expect(sessionActivityIndicator.locator("svg")).toHaveCount(1);
    await expect(sessionActivityIndicator.locator("svg")).toHaveCSS(
      "animation-name",
      "session-activity-icon-spin",
    );
    await expect.poll(() => sessionVisual.evaluate((element) => {
      return window.getComputedStyle(element, "::after").content;
    })).toBe("none");
    const runningSurface = await sessionRow.evaluate((element) => {
      const surface = window.getComputedStyle(element);
      return {
        backgroundColor: surface.backgroundColor,
        animationName: surface.animationName,
      };
    });
    expect(runningSurface.backgroundColor).toMatch(/^(rgb|color\(|oklab\(|oklch\()/);
    expect(runningSurface.animationName).toBe("none");
    const runningLight = await sessionRow.evaluate((element) => {
      const light = window.getComputedStyle(element, "::before");
      return {
        animationName: light.animationName,
        backgroundImage: light.backgroundImage,
        content: light.content,
      };
    });
    expect(runningLight.content).toBe("none");
    expect(runningLight.animationName).toBe("none");
    expect(runningLight.backgroundImage).toBe("none");
    const staticSurfaceColors: string[] = [];
    for (const expected of [
      "activity-completed",
      "activity-attention",
      "activity-error",
    ]) {
      await sessionRow.evaluate(async (element, className) => {
        element.classList.remove("activity-running", "activity-completed", "activity-attention", "activity-error");
        element.classList.add(className);
        // Force the new style to resolve, then wait for the row's background transition.
        window.getComputedStyle(element).backgroundColor;
        await new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()));
        await Promise.all(element.getAnimations().map((animation) => animation.finished.catch(() => undefined)));
      }, expected);
      const staticSurface = await sessionRow.evaluate((element) => {
        const surface = window.getComputedStyle(element);
        return { backgroundColor: surface.backgroundColor, animationName: surface.animationName };
      });
      expect(staticSurface.backgroundColor).toMatch(/^(rgb|color\(|oklab\(|oklch\()/);
      expect(staticSurface.animationName).toBe("none");
      staticSurfaceColors.push(staticSurface.backgroundColor);
    }
    expect(new Set(staticSurfaceColors).size).toBe(3);
    await sessionRow.evaluate((element) => {
      element.classList.remove("activity-completed", "activity-attention", "activity-error");
      element.classList.add("activity-running");
    });
    await expect.poll(() => sessionRow.evaluate((element) => (
      window.getComputedStyle(element, "::before").animationName
    ))).toBe("none");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await expect.poll(() => sessionRow.evaluate((element) => (
      window.getComputedStyle(element, "::before").animationName
    ))).toBe("none");
    await expect(sessionActivityIndicator.locator("svg")).toHaveCSS("animation-name", "none");
    await page.emulateMedia({ reducedMotion: "no-preference" });
    if (testInfo.project.name === "chromium") {
      await page.getByRole("button", { name: "Collapse sidebar" }).click();
      const collapsedSessionButton = page.locator(".collapsed-session-list .selected-session-dot");
      await expect(collapsedSessionButton).toBeVisible();
      await expect(collapsedSessionButton).toHaveClass(/activity-running/);
      const collapsedSurface = await collapsedSessionButton.evaluate((element) => {
        const surface = window.getComputedStyle(element);
        return { backgroundColor: surface.backgroundColor, animationName: surface.animationName };
      });
      expect(collapsedSurface.backgroundColor).toMatch(/^(rgb|color\(|oklab\(|oklch\()/);
      expect(collapsedSurface.animationName).toBe("none");
      await expect.poll(() => collapsedSessionButton.evaluate((element) => (
        window.getComputedStyle(element, "::before").animationName
      ))).toBe("none");
      await page.getByRole("button", { name: "Expand sidebar" }).click();
    }
    if (testInfo.project.name === "chromium") {
      await page.screenshot({ path: "test-results/session-activity-row-desktop.png", fullPage: true });
      await sessionRow.getByRole("button", { name: "Close session" }).click();
      const closeSessionDialog = page.getByRole("alertdialog", { name: "Close session?" });
      await expect(closeSessionDialog).toBeVisible();
      await closeSessionDialog.getByRole("button", { name: "Cancel" }).click();
      await expect(closeSessionDialog).toBeHidden();
    }

    await openSessionButton.click();
    await expectTerminalLine(page, "termd-e2e-ready", 8_000);

    if (testInfo.project.name !== "mobile-chrome") {
      daemon.pushSessionData(
        "00000000-0000-0000-0000-000000000501",
        Array.from({ length: 96 }, (_, index) => `resize-scroll-bottom-${index}\n`).join(""),
      );
      await expectTerminalLine(page, "resize-scroll-bottom-95", 8_000);
      const resizer = page.getByRole("separator", { name: "Resize files panel" });
      const box = await resizer.boundingBox();
      expect(box).not.toBeNull();
      await page.mouse.move((box?.x ?? 0) + (box?.width ?? 1) / 2, (box?.y ?? 0) + 20);
      await page.mouse.down();
      await page.mouse.move((box?.x ?? 0) - 120, (box?.y ?? 0) + 20);
      await page.mouse.up();
      await terminalPane.click();
      await expectTerminalLine(page, "resize-scroll-bottom-95", 8_000);
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
      await expect(page.getByRole("dialog", { name: "sessions panel" })).toBeHidden();
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

    await focusTerminalKeyboardSink(page);
    if (testInfo.project.name === "mobile-chrome") {
      // 缩放/viewer 模式已经移除；移动端只验证终端本体没有退回旧的缩放控件。
      await expect(page.getByRole("button", { name: /zoom/i })).toHaveCount(0);
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
    expectPairingTokenOnlyInRelayAdmission(daemon);

    if (testInfo.project.name === "mobile-chrome") {
      await page.evaluate(() => {
        // 移动端软键盘收起或输入框 blur 后，终端不能被同步成一行或丢失可视高度。
        if (document.activeElement instanceof HTMLElement) {
          document.activeElement.blur();
        }
      });
      await expect(page.getByRole("button", { name: /zoom/i })).toHaveCount(0);
      await expect
        .poll(async () => (await terminalPane.boundingBox())?.height ?? 0)
        .toBeGreaterThan(280);
    }

    await page.reload();
    await expectTerminalLine(page, "termd-e2e-ready", 8_000);
    const reloadedProgressButton = page
      .getByTestId("terminal-pane")
      .getByRole("button", { name: "Session opening progress" });
    await expect(reloadedProgressButton).toBeVisible();
    await expect(reloadedProgressButton.locator("svg[data-open-progress-icon='ready']")).toBeVisible();
    await focusTerminalKeyboardSink(page);
    await page.keyboard.type("terminal-after-reload");
    await page.keyboard.press("Enter");
    await expect
      .poll(() => daemon.decryptedInputs.join(""))
      .toContain("terminal-after-reload");
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

test("wide coarse-pointer workspace keeps short-height rows and header controls contained", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "coarse pointer layout only");
  const sessionId = "00000000-0000-0000-0000-00000000050a";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "short-workspace-ready\n",
    sessionFiles: {
      [sessionId]: {
        session_id: sessionId,
        path: "/workspace",
        entries: [],
      },
    },
  });

  try {
    await page.setViewportSize({ width: 844, height: 390 });
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "short-workspace-ready", 8_000);
    await activateButton(page, "Show files panel");
    await expect(page.locator(".files-panel-header")).toBeVisible();

    const geometry = await page.locator(".workspace").evaluate((workspace) => {
      const rectOf = (selector: string) => {
        const element = workspace.querySelector<HTMLElement>(selector);
        if (!element) return undefined;
        const rect = element.getBoundingClientRect();
        return { top: rect.top, bottom: rect.bottom, height: rect.height };
      };
      const controlsInside = (containerSelector: string) => {
        const container = workspace.querySelector<HTMLElement>(containerSelector);
        if (!container) return undefined;
        const containerRect = container.getBoundingClientRect();
        return [...container.querySelectorAll<HTMLElement>("button")]
          .filter((button) => button.getClientRects().length > 0)
          .map((button) => {
            const rect = button.getBoundingClientRect();
            return {
              height: rect.height,
              contained: rect.top >= containerRect.top - 0.5 && rect.bottom <= containerRect.bottom + 0.5,
            };
          });
      };
      const workspaceRect = workspace.getBoundingClientRect();
      return {
        viewportHeight: window.innerHeight,
        workspace: { top: workspaceRect.top, bottom: workspaceRect.bottom },
        toolbar: rectOf(".toolbar"),
        body: rectOf(".workspace-body"),
        status: rectOf(".daemon-status-strip"),
        toolbarControls: controlsInside(".toolbar"),
        filesHeaderControls: controlsInside(".files-panel-header"),
      };
    });

    expect(geometry.workspace.top).toBeGreaterThanOrEqual(0);
    expect(geometry.workspace.bottom).toBeLessThanOrEqual(geometry.viewportHeight);
    expect(geometry.toolbar?.bottom).toBeCloseTo(geometry.body?.top ?? -1, 0);
    expect(geometry.body?.bottom).toBeCloseTo(geometry.status?.top ?? -1, 0);
    expect(geometry.status?.bottom).toBeLessThanOrEqual(geometry.workspace.bottom);
    expect(geometry.body?.height ?? 0).toBeGreaterThan(0);
    expect(geometry.toolbarControls?.length ?? 0).toBeGreaterThan(0);
    expect(geometry.filesHeaderControls?.length ?? 0).toBeGreaterThan(0);
    expect(geometry.toolbarControls?.every(({ height, contained }) => height >= 44 && contained)).toBe(true);
    expect(geometry.filesHeaderControls?.every(({ height, contained }) => height >= 44 && contained)).toBe(true);
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
    await expectTerminalLine(page, "direct-slow-ready", 8_000);
    // 中文注释：files/status 都是非终端 segment；超过普通 UI deadline 后，
    // 页面应只把对应 panel 标成不可用，terminal stream 仍保持可输入。
    await expect(page.getByLabel("session files").getByText("unavailable")).toBeVisible({ timeout: 8_000 });
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);

    await terminalPane.click();
    await focusTerminalKeyboardSink(page);
    await page.keyboard.type("direct-after-timeout");
    await page.keyboard.press("Enter");
    await expect.poll(() => daemon.decryptedInputs.join("")).toContain("direct-after-timeout");
    // 中文注释：0.6.0 起 metadata/control 可以和 terminal attach 分离，慢 RPC 超时后
    // 最多保留一条终端连接加一条 metadata/control 连接即可，不能继续堆积。
    expect(daemon.activeConnectionCount()).toBeLessThanOrEqual(2);
  } finally {
    await daemon.stop();
  }
});

test("移动端终端触摸滚动遵循内容跟手语义", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "该回归只需要移动端项目覆盖");

  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-000000000531",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: `${Array.from({ length: 320 }, (_, index) => `${String(index + 1).padStart(4, "0")}\n`).join("")}mobile-scroll-ready\n`,
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "mobile-scroll-ready", 8_000);
    await waitForStableTerminalSurface(page);

    const initialState = await terminalViewportState(page);
    expect(initialState.scrollbackLength).toBeGreaterThan(0);
    expect(initialState.viewportRaw).toBe(0);

    const frame = page.locator(".terminal-frame");
    await frame.dispatchEvent("pointerdown", {
      pointerId: 51,
      pointerType: "touch",
      button: 0,
      clientX: 180,
      clientY: 420,
    });
    await frame.dispatchEvent("pointermove", {
      pointerId: 51,
      pointerType: "touch",
      buttons: 1,
      clientX: 180,
      clientY: 520,
    });
    await frame.dispatchEvent("pointerup", {
      pointerId: 51,
      pointerType: "touch",
      button: 0,
      clientX: 180,
      clientY: 520,
    });

    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 8_000 })
      .toBeGreaterThan(0);

    const historyViewportRaw = await terminalViewportState(page).then((state) => state.viewportRaw);
    const scrolledLines = await terminalViewportNumberLines(page);
    expect(scrolledLines.length).toBeGreaterThan(8);
    for (let index = 1; index < scrolledLines.length; index += 1) {
      // 中文注释：移动端触摸滚动后，当前 viewport 里仍必须是顺序连续的历史行。
      expect(scrolledLines[index]).toBe(scrolledLines[index - 1] + 1);
    }

    await frame.dispatchEvent("pointerdown", {
      pointerId: 52,
      pointerType: "touch",
      button: 0,
      clientX: 180,
      clientY: 520,
    });
    await frame.dispatchEvent("pointermove", {
      pointerId: 52,
      pointerType: "touch",
      buttons: 1,
      clientX: 180,
      clientY: 420,
    });
    await frame.dispatchEvent("pointerup", {
      pointerId: 52,
      pointerType: "touch",
      button: 0,
      clientX: 180,
      clientY: 420,
    });

    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 8_000 })
      .toBeLessThan(historyViewportRaw);
  } finally {
    await daemon.stop();
  }
});

test("direct Web 多个大输出 session 快速切换后仍贴底并能输入", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "多 session 输出切换回归只需要桌面布局覆盖");
  const firstSessionId = "00000000-0000-0000-0000-000000000521";
  const secondSessionId = "00000000-0000-0000-0000-000000000522";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: firstSessionId,
        name: "Direct Alpha",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
      {
        session_id: secondSessionId,
        name: "Direct Beta",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "direct-attach-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    const terminalPane = page.getByTestId("terminal-pane");
    await openSession(page, "Direct Alpha");
    await expect.poll(() => daemon.attachedSessions.includes(firstSessionId)).toBe(true);
    await expectTerminalLine(page, "direct-attach-ready", 8_000);
    daemon.pushSessionData(
      firstSessionId,
      Array.from({ length: 180 }, (_, index) => `direct-alpha-bulk-${index}\n`).join("") + "direct-alpha-ready\n",
    );
    await expectTerminalLine(page, "direct-alpha-ready", 8_000);

    await openSession(page, "Direct Beta");
    await expect.poll(() => daemon.attachedSessions.includes(secondSessionId)).toBe(true);
    await expectTerminalLine(page, "direct-attach-ready", 8_000);
    daemon.pushSessionData(
      secondSessionId,
      Array.from({ length: 180 }, (_, index) => `direct-beta-bulk-${index}\n`).join("") + "direct-beta-ready\n",
    );
    await expectTerminalLine(page, "direct-beta-ready", 8_000);

    // 中文注释：快速切换后旧 session 的 backlog 不能挡住当前 session 的最后输出。
    for (let round = 0; round < 10; round += 1) {
      await openSession(page, round % 2 === 0 ? "Direct Alpha" : "Direct Beta");
    }
    await openSession(page, "Direct Beta");
    await expect.poll(() => daemon.attachedSessions.at(-1)).toBe(secondSessionId);
    await expectTerminalLine(page, "direct-attach-ready", 8_000);
    daemon.pushSessionData(secondSessionId, "direct-beta-tail-after-switch\n");
    await expectTerminalLine(page, "direct-beta-tail-after-switch", 8_000);
    await expectTerminalScrollAtBottom(page);

    const resizer = page.getByRole("separator", { name: "Resize files panel" });
    const box = await resizer.boundingBox();
    expect(box).not.toBeNull();
    await page.mouse.move((box?.x ?? 0) + (box?.width ?? 1) / 2, (box?.y ?? 0) + 20);
    await page.mouse.down();
    await page.mouse.move((box?.x ?? 0) - 120, (box?.y ?? 0) + 20);
    await page.mouse.up();
    await expectTerminalScrollAtBottom(page);

    await terminalPane.click();
    await focusTerminalKeyboardSink(page);
    await page.keyboard.type("direct-switch-input-ok");
    await page.keyboard.press("Enter");
    await expect.poll(() => daemon.decryptedInputs.join("")).toContain("direct-switch-input-ok");
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
  } finally {
    await daemon.stop();
  }
});

test("direct Web 新建 session 切走再切回后不会重复发送输入", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "输入重复回归只需要桌面布局覆盖");
  const existingSessionId = "00000000-0000-0000-0000-000000000541";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: existingSessionId,
        name: "Switch Anchor",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "switch-attach-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    const createdName = await createShellSession(page, ["Switch Anchor"]);
    await expectTerminalLine(page, "switch-attach-ready", 8_000);
    await openSession(page, "Switch Anchor");
    await expect.poll(() => daemon.attachedSessions.at(-1)).toBe(existingSessionId);
    await expectTerminalLine(page, "switch-attach-ready", 8_000);
    await openSession(page, createdName);
    await expect.poll(() => daemon.attachedSessions.at(-1)).not.toBe(existingSessionId);
    await expectTerminalLine(page, "switch-attach-ready", 8_000);

    const createdSessionId = "00000000-0000-0000-0000-000000000501";
    daemon.pushSessionData(createdSessionId, "switch-back-output-once\n");
    await expectTerminalLine(page, "switch-back-output-once", 8_000);
    const terminalBuffer = await terminalDebugBufferText(page);
    expect(countOccurrences(terminalBuffer, "switch-back-output-once")).toBe(1);

    const inputBaselineLength = daemon.decryptedInputs.length;
    await focusTerminalKeyboardSink(page);
    await page.keyboard.type("switch-back-echo-once");
    await page.keyboard.press("Enter");

    await expect
      .poll(() => daemon.decryptedInputs.slice(inputBaselineLength).join(""))
      .toContain("switch-back-echo-once\r");
    // 中文注释：用户报告的“多一个回显”首先要排除同一按键被多条 onData/attach
    // 链路重复送到 supervisor；mock daemon 记录的是已解密的真实 stdin 片段。
    const sentInput = daemon.decryptedInputs.slice(inputBaselineLength).join("");
    expect(countOccurrences(sentInput, "switch-back-echo-once")).toBe(1);
  } finally {
    await daemon.stop();
  }
});

test("terminal wheel 向上滚动会朝更旧的历史移动", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "滚轮方向回归只需要桌面布局覆盖");
  const sessionId = "00000000-0000-0000-0000-000000000531";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: Array.from({ length: 500 }, (_, index) => `${String(index + 1).padStart(3, "0")}\n`).join(""),
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    await expectTerminalLine(page, "500", 8_000);
    const terminalPane = page.getByTestId("terminal-pane");
    await terminalPane.hover();

    const initialState = await terminalViewportState(page);
    expect(initialState.scrollbackLength).toBeGreaterThan(0);
    expect(initialState.viewportRaw).toBe(0);

    const initialTopLine = initialState.scrollbackLength - initialState.viewportRaw + 1;
    await page.mouse.wheel(0, -900);
    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 8_000 })
      .toBeGreaterThan(0);

    const scrolledState = await terminalViewportState(page);
    const scrolledTopLine = scrolledState.scrollbackLength - scrolledState.viewportRaw + 1;
    // 中文注释：wheel 往上滚时，viewport 应朝更旧的历史移动，因此顶部可见行号必须变小。
    expect(scrolledTopLine).toBeLessThan(initialTopLine);
  } finally {
    await daemon.stop();
  }
});

test("terminal 上滚后 1..1000 历史顺序和下半区拖拽复制一致", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "scrollback 视觉/复制坐标回归先覆盖桌面布局");
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  const sessionId = "00000000-0000-0000-0000-000000000532";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: Array.from({ length: 1000 }, (_, index) => `${String(index + 1).padStart(4, "0")}\n`).join(""),
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    await expectTerminalLine(page, "1000", 8_000);
    await focusTerminalKeyboardSink(page);
    await waitForStableTerminalSurface(page);
    await page.getByTestId("terminal-pane").hover();
    await page.mouse.wheel(0, -1400);
    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 8_000 })
      .toBeGreaterThan(0);

    const viewportLines = await terminalViewportNumberLines(page);
    expect(viewportLines.length).toBeGreaterThan(10);
    for (let index = 1; index < viewportLines.length; index += 1) {
      // 中文注释：用户反馈 “从 1 打印到 1000 后，上滚看到的数字顺序乱掉”；
      // 当前 viewport 必须仍是逐行递增的历史内容，不能出现旧表面残留。
      expect(viewportLines[index]).toBe(viewportLines[index - 1] + 1);
    }

    await waitForStableTerminalSurface(page);
    const metrics = await terminalSurfaceMetrics(page);
    expect(metrics.rows).toBeGreaterThan(10);
    expect(metrics.cols).toBeGreaterThan(20);

    const targetRow = Math.min(metrics.rows - 3, Math.max(Math.floor(metrics.rows * 0.68), 8));
    const expectedLine = (await terminalViewportText(page)).split("\n")[targetRow]?.trim() ?? "";
    expect(expectedLine).toMatch(/^\d{4}$/);

    const cellWidth = metrics.width / metrics.cols;
    const cellHeight = metrics.height / metrics.rows;
    const startX = metrics.left + cellWidth * 0.2;
    const endX = metrics.left + cellWidth * 3.8;
    const y = metrics.top + cellHeight * (targetRow + 0.55);
    await page.mouse.move(startX, y);
    await page.mouse.down();
    await page.mouse.move(endX, y);
    await page.mouse.up();

    await expect
      .poll(async () => page.evaluate(() => navigator.clipboard.readText()), { timeout: 2_000 })
      .toContain(expectedLine);
    const selectionCopy = await page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdSelectionCopy ?? "");
    // 中文注释：复制文本也必须来自当前 viewport 的目标行；不能发生“看到下半区，复制上半区”的坐标分裂。
    expect(selectionCopy).toContain(expectedLine);
  } finally {
    await daemon.stop();
  }
});

test("terminal 选区存在时 Ctrl+C 会复制选区而不是向 PTY 发送中断", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "快捷键复制回归先覆盖桌面布局");
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  const sessionId = "00000000-0000-0000-0000-000000000533";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: Array.from({ length: 120 }, (_, index) => `copy-${String(index + 1).padStart(3, "0")}\n`).join(""),
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    await expectTerminalLine(page, "copy-120", 8_000);
    await page.getByTestId("terminal-pane").hover();

    const metrics = await terminalSurfaceMetrics(page);
    expect(metrics.rows).toBeGreaterThan(5);
    expect(metrics.cols).toBeGreaterThan(10);

    const targetRow = Math.min(metrics.rows - 4, Math.max(6, Math.floor(metrics.rows * 0.6)));
    const expectedLine = (await terminalViewportText(page)).split("\n")[targetRow]?.trim() ?? "";
    expect(expectedLine).toMatch(/^copy-\d{3}$/);
    const selectedLine = await page.evaluate(
      ({ row, endCol }) => {
        const scope = window as typeof window & {
          __TERMD_DEBUG_TERMINAL__?: {
            selectViewportRange: (
              start: { col: number; row: number },
              end: { col: number; row: number },
            ) => string | undefined;
          };
        };
        return scope.__TERMD_DEBUG_TERMINAL__?.selectViewportRange(
          { col: 0, row },
          { col: endCol, row },
        ) ?? "";
      },
      {
        row: targetRow,
        // 中文注释：只选实际文本列，避免把右侧空白区带进选择结果。
        endCol: Math.max(0, expectedLine.length - 1),
      },
    );
    expect(selectedLine).toBe(expectedLine);

    await expect
      .poll(
        async () => page.locator(".terminal-host").evaluate((host) => ({
          hasSelection: (host as HTMLElement).dataset.termdHasSelection ?? "",
          selection: (host as HTMLElement).dataset.termdSelection ?? "",
        })),
        { timeout: 2_000 },
      )
      .toMatchObject({
        hasSelection: "true",
        selection: expectedLine,
      });

    await page.evaluate(() => navigator.clipboard.writeText("clipboard-reset"));
    const sessionDataCountBeforeCopy = daemon.sessionDataMessages.length;
    await page.keyboard.press("Control+C");

    await expect
      .poll(async () => page.evaluate(() => navigator.clipboard.readText()), { timeout: 2_000 })
      .toContain(expectedLine);
    await page.waitForTimeout(250);
    expect(daemon.sessionDataMessages.slice(sessionDataCountBeforeCopy)).toEqual([]);
  } finally {
    await daemon.stop();
  }
});

test("terminal reload 后只向 daemon 上报最终稳定尺寸", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "reload 尺寸稳定回归先覆盖桌面布局");
  const sessionId = "00000000-0000-0000-0000-000000000541";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "reload-resize-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "reload-resize-ready", 8_000);
    await focusTerminalKeyboardSink(page);

    await expect
      .poll(() => daemon.sessionResizes.length, { timeout: 8_000 })
      .toBeGreaterThan(0);
    const initialResizeCount = daemon.sessionResizes.length;

    // 中文注释：用户反馈刷新时肉眼能看到分辨率跳很多次；这里记录真实浏览器
    // 内 xterm 的 rows/cols 变化，同时断言 shared PTY 只收到最终稳定尺寸。
    await page.addInitScript(() => {
      const scope = window as typeof window & {
        __termdReloadTerminalSizes?: string[];
      };
      scope.__termdReloadTerminalSizes = [];
      let hostObserver: MutationObserver | undefined;
      const record = () => {
        const host = document.querySelector<HTMLElement>(".terminal-host");
        const rows = host?.dataset.termdRows;
        const cols = host?.dataset.termdCols;
        if (!rows || !cols) {
          return;
        }
        const key = `${cols}x${rows}`;
        if (scope.__termdReloadTerminalSizes?.at(-1) !== key) {
          scope.__termdReloadTerminalSizes?.push(key);
        }
      };
      const attachHostObserver = () => {
        const host = document.querySelector<HTMLElement>(".terminal-host");
        if (!host || hostObserver) {
          return;
        }
        hostObserver = new MutationObserver(record);
        hostObserver.observe(host, { attributes: true, attributeFilter: ["data-termd-cols", "data-termd-rows"] });
        record();
      };
      const treeObserver = new MutationObserver(() => {
        attachHostObserver();
      });
      window.addEventListener("DOMContentLoaded", () => {
        attachHostObserver();
        treeObserver.observe(document.documentElement, { childList: true, subtree: true });
      });
      window.addEventListener("load", record);
    });
    await page.reload();
    await expectTerminalLine(page, "reload-resize-ready", 8_000);
    await focusTerminalKeyboardSink(page);
    await page.waitForTimeout(500);

    const reloadResizes = daemon.sessionResizes.slice(initialResizeCount);
    const uniqueReloadDaemonSizes = Array.from(
      new Set(reloadResizes.map((entry) => `${entry.size.cols}x${entry.size.rows}`)),
    );
    const terminalSizeSequence = await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdReloadTerminalSizes?: string[];
      };
      return scope.__termdReloadTerminalSizes ?? [];
    });

    expect(terminalSizeSequence.length).toBeGreaterThan(0);
    expect(uniqueReloadDaemonSizes.length).toBeLessThanOrEqual(1);
    if (uniqueReloadDaemonSizes.length === 1) {
      const finalxtermSize = terminalSizeSequence.at(-1);
      expect(uniqueReloadDaemonSizes[0]).toBe(finalxtermSize);
    }
    expect(terminalSizeSequence.length).toBeLessThanOrEqual(2);
  } finally {
    await daemon.stop();
  }
});

test("terminal 进入后台标签页时仍持续消费输出，不依赖前台 requestAnimationFrame", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "后台 tab drain 回归先覆盖桌面布局");
  test.setTimeout(60_000);
  const sessionId = "00000000-0000-0000-0000-000000000542";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "background-tab-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "background-tab-ready", 8_000);

    const secondPage = await page.context().newPage();
    await secondPage.goto("about:blank");
    await secondPage.bringToFront();

    daemon.pushSessionData(sessionId, "background-tab-live-output\n");
    await expect
      .poll(async () => terminalDebugBufferText(page), { timeout: 20_000 })
      .toContain("background-tab-live-output");
    // 中文注释：后台 tab 仍允许 metadata/control 连接存在；这里约束没有反复重连泄漏。
    expect(daemon.activeConnectionCount()).toBeLessThanOrEqual(2);

    await secondPage.close();
    await page.bringToFront();
    await expectTerminalLine(page, "background-tab-live-output", 20_000);
  } finally {
    await daemon.stop();
  }
});

test("terminal 在前台已排队的 write callback 切到后台后仍会继续推进", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "后台 write rescue 回归先覆盖桌面布局");
  const sessionId = "00000000-0000-0000-0000-000000000544";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "background-writer-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "background-writer-ready", 8_000);

    await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdHeldWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      };
      const queuedFrames = new Map<number, FrameRequestCallback>();
      let nextFrameId = 1;
      scope.__termdHeldWriteRaf = {
        pendingCount: () => queuedFrames.size,
        runNext: () => {
          const nextFrame = queuedFrames.entries().next();
          if (nextFrame.done) {
            return false;
          }
          const [frameId, callback] = nextFrame.value;
          queuedFrames.delete(frameId);
          callback(performance.now());
          return true;
        },
      };
      (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__ = {
        schedule: (callback: () => void) => {
          const frameId = nextFrameId;
          nextFrameId += 1;
          queuedFrames.set(frameId, () => {
            callback();
          });
          return frameId;
        },
        cancel: (frameId: number) => {
          queuedFrames.delete(Number(frameId));
        },
      };
    });

    daemon.pushSessionData(sessionId, "background-writer-race-output\n");
    await expect
      .poll(async () =>
        page.evaluate(() => {
          const scope = window as typeof window & {
            __termdHeldWriteRaf?: { pendingCount: () => number };
          };
          return scope.__termdHeldWriteRaf?.pendingCount() ?? 0;
        }), { timeout: 8_000 })
      .toBeGreaterThan(0);
    await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdHeldWriteRaf?: { runNext: () => boolean };
      };
      scope.__termdHeldWriteRaf?.runNext();
    });

    daemon.pushSessionData(sessionId, "background-writer-race-output-2\n");
    await expect
      .poll(async () =>
        page.evaluate(() => {
          const scope = window as typeof window & {
            __termdHeldWriteRaf?: { pendingCount: () => number };
          };
          return scope.__termdHeldWriteRaf?.pendingCount() ?? 0;
        }), { timeout: 8_000 })
      .toBeGreaterThan(0);

    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      Object.defineProperty(document, "hidden", {
        configurable: true,
        get: () => true,
      });
      window.dispatchEvent(new Event("blur"));
      document.dispatchEvent(new Event("visibilitychange"));
    });

    await expect
      .poll(async () => terminalDebugBufferText(page), { timeout: 8_000 })
      .toContain("background-writer-race-output-2");
  } finally {
    await page.evaluate(() => {
      Reflect.deleteProperty(document, "visibilityState");
      Reflect.deleteProperty(document, "hidden");
      window.dispatchEvent(new Event("focus"));
      document.dispatchEvent(new Event("visibilitychange"));
      delete (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
        __termdHeldWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
      delete (window as typeof window & {
        __termdHeldWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      }).__termdHeldWriteRaf;
    });
    await daemon.stop();
  }
});

test("terminal 在前台已排队的 write callback 切到 blur 但仍 visible 后仍会继续推进", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "blur rescue 回归先覆盖桌面布局");
  const sessionId = "00000000-0000-0000-0000-000000000546";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "blur-writer-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "blur-writer-ready", 8_000);

    await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdHeldBlurWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      };
      const queuedFrames = new Map<number, FrameRequestCallback>();
      let nextFrameId = 1;
      scope.__termdHeldBlurWriteRaf = {
        pendingCount: () => queuedFrames.size,
        runNext: () => {
          const nextFrame = queuedFrames.entries().next();
          if (nextFrame.done) {
            return false;
          }
          const [frameId, callback] = nextFrame.value;
          queuedFrames.delete(frameId);
          callback(performance.now());
          return true;
        },
      };
      (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__ = {
        schedule: (callback: () => void) => {
          const frameId = nextFrameId;
          nextFrameId += 1;
          queuedFrames.set(frameId, () => {
            callback();
          });
          return frameId;
        },
        cancel: (frameId: number) => {
          queuedFrames.delete(Number(frameId));
        },
      };
    });

    daemon.pushSessionData(sessionId, "blur-writer-race-output\n");
    await expect
      .poll(async () =>
        page.evaluate(() => {
          const scope = window as typeof window & {
            __termdHeldBlurWriteRaf?: { pendingCount: () => number };
          };
          return scope.__termdHeldBlurWriteRaf?.pendingCount() ?? 0;
        }), { timeout: 8_000 })
      .toBeGreaterThan(0);
    await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdHeldBlurWriteRaf?: { runNext: () => boolean };
      };
      scope.__termdHeldBlurWriteRaf?.runNext();
    });

    daemon.pushSessionData(sessionId, "blur-writer-race-output-2\n");
    await expect
      .poll(async () =>
        page.evaluate(() => {
          const scope = window as typeof window & {
            __termdHeldBlurWriteRaf?: { pendingCount: () => number };
          };
          return scope.__termdHeldBlurWriteRaf?.pendingCount() ?? 0;
        }), { timeout: 8_000 })
      .toBeGreaterThan(0);

    await page.evaluate(() => {
      Object.defineProperty(document, "hasFocus", {
        configurable: true,
        value: () => false,
      });
      window.dispatchEvent(new Event("blur"));
    });

    await expect
      .poll(async () => terminalDebugBufferText(page), { timeout: 8_000 })
      .toContain("blur-writer-race-output-2");
  } finally {
    await page.evaluate(() => {
      Reflect.deleteProperty(document, "hasFocus");
      window.dispatchEvent(new Event("focus"));
      delete (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
        __termdHeldBlurWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
      delete (window as typeof window & {
        __termdHeldBlurWriteRaf?: {
          pendingCount: () => number;
          runNext: () => boolean;
        };
      }).__termdHeldBlurWriteRaf;
    });
    await daemon.stop();
  }
});

test("terminal 在 blur 但仍 visible 时收到新输出也不依赖 rAF", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "blur 直退 timer 的回归先覆盖桌面布局");
  const sessionId = "00000000-0000-0000-0000-000000000545";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "blur-direct-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "blur-direct-ready", 8_000);

    await page.evaluate(() => {
      const heldFrames = new Map<number, () => void>();
      let nextFrameId = 1;
      const holdFrame = (callback: () => void) => {
        const frameId = nextFrameId;
        nextFrameId += 1;
        heldFrames.set(frameId, callback);
        return frameId;
      };
      const cancelFrame = (frameId: number) => {
        heldFrames.delete(Number(frameId));
      };
      (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__ = {
        schedule: holdFrame,
        cancel: cancelFrame,
      };
      (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__ = {
        schedule: holdFrame,
        cancel: cancelFrame,
      };
      Object.defineProperty(document, "hasFocus", {
        configurable: true,
        value: () => false,
      });
      window.dispatchEvent(new Event("blur"));
    });

    daemon.pushSessionData(sessionId, "blur-direct-live-output\n");
    await expect
      .poll(async () => terminalDebugBufferText(page), { timeout: 8_000 })
      .toContain("blur-direct-live-output");
  } finally {
    await page.evaluate(() => {
      Reflect.deleteProperty(document, "hasFocus");
      window.dispatchEvent(new Event("focus"));
      delete (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__;
      delete (window as typeof window & {
        __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
          schedule: (callback: () => void) => number;
          cancel: (handle: number) => void;
        };
      }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
    });
    await daemon.stop();
  }
});

test("terminal 从后台标签页回到前台并重新聚焦时 rows/cols 保持稳定，不闪回远端网格", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "回焦尺寸稳定回归先覆盖桌面布局");
  const sessionId = "00000000-0000-0000-0000-000000000543";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "focus-return-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expectTerminalLine(page, "focus-return-ready", 8_000);
    await focusTerminalKeyboardSink(page);

    await expect
      .poll(async () => {
        const size = await terminalHostSize(page);
        return size.cols > 80 && size.rows > 24;
      }, { timeout: 8_000 })
      .toBe(true);
    const stableSize = await terminalHostSize(page);

    await page.evaluate(() => {
      const scope = window as typeof window & { __termdFocusReturnSizes?: string[] };
      scope.__termdFocusReturnSizes = [];
      const host = document.querySelector<HTMLElement>(".terminal-host");
      if (!host) {
        return;
      }
      const record = () => {
        const rows = host.dataset.termdRows;
        const cols = host.dataset.termdCols;
        if (!rows || !cols) {
          return;
        }
        const key = `${cols}x${rows}`;
        if (scope.__termdFocusReturnSizes?.at(-1) !== key) {
          scope.__termdFocusReturnSizes?.push(key);
        }
      };
      const observer = new MutationObserver(record);
      observer.observe(host, { attributes: true, attributeFilter: ["data-termd-cols", "data-termd-rows"] });
      record();
      (window as typeof window & { __termdFocusReturnSizeObserver?: MutationObserver }).__termdFocusReturnSizeObserver = observer;
    });

    const secondPage = await page.context().newPage();
    await secondPage.goto("about:blank");
    await secondPage.bringToFront();
    await page.waitForTimeout(300);

    await page.bringToFront();
    await page.locator(".terminal-frame").click();
    await focusTerminalKeyboardSink(page);
    await page.waitForTimeout(600);

    const sizeSequence = await page.evaluate(() => {
      const scope = window as typeof window & { __termdFocusReturnSizes?: string[] };
      return scope.__termdFocusReturnSizes ?? [];
    });
    const uniqueSizes = Array.from(new Set(sizeSequence));
    expect(uniqueSizes).toEqual([`${stableSize.cols}x${stableSize.rows}`]);

    await page.evaluate(() => {
      const scope = window as typeof window & { __termdFocusReturnSizeObserver?: MutationObserver };
      scope.__termdFocusReturnSizeObserver?.disconnect();
      delete scope.__termdFocusReturnSizeObserver;
    });
    await secondPage.close();
  } finally {
    await daemon.stop();
  }
});

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

function expectPairingTokenOnlyInRelayAdmission(daemon: MockDaemon, token = "secret-token"): void {
  // 中文注释：0.7 pairing 走一次标准 HTTP 请求；pair ticket 只出现在该请求的
  // Authorization 中，metadata/terminal WebSocket 和后续业务帧都不能携带它。
  expect(daemon.pairingTokens).toEqual([token]);
  expect(daemon.outerWireText()).not.toContain(token);
}

async function openSession(page: Page, name: string): Promise<void> {
  await page.getByRole("button", { name: `Open ${name}` }).click();
}

async function createShellSession(page: Page, existingNames: string[]): Promise<string> {
  await page.getByRole("button", { name: "New session" }).click();
  await expect(page.getByRole("textbox", { name: "Terminal input" })).toBeAttached({ timeout: 8_000 });
  await expect
    .poll(async () => sessionNames(page), { timeout: 8_000 })
    .toHaveLength(existingNames.length + 1);
  const names = await sessionNames(page);
  const created = names.find((name) => !existingNames.includes(name));
  if (!created) {
    throw new Error(`failed to detect created session from ${names.join(", ")}`);
  }
  return created;
}

async function sessionNames(page: Page): Promise<string[]> {
  const openButtons = await page
    .getByRole("region", { name: "sessions" })
    .getByRole("button", { name: /^Open / })
    .all();
  const names: string[] = [];
  for (const button of openButtons) {
    const label = await button.getAttribute("aria-label");
    const name = label?.replace(/^Open /u, "").replace(/, new output$/u, "").trim();
    if (name) {
      names.push(name);
    }
  }
  return names;
}

function countOccurrences(input: string, needle: string): number {
  let count = 0;
  let index = 0;
  while (index < input.length) {
    const found = input.indexOf(needle, index);
    if (found === -1) {
      return count;
    }
    count += 1;
    index = found + needle.length;
  }
  return count;
}

async function expectTerminalLine(page: Page, text: string, timeout: number): Promise<void> {
  // 中文注释：xterm 的真实绘制层不适合作为稳定断言面；E2E build 显式开启安全的
  // data-termd-buffer 镜像，供浏览器测试验证终端内容。
  await expect
    .poll(async () => terminalDebugBufferText(page), { timeout })
    .toContain(text);
}

async function terminalDebugBufferText(page: Page): Promise<string> {
  return page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? "");
}

async function enableTermdDiagnostics(page: Page): Promise<void> {
  await page.addInitScript(() => {
    // 中文注释：移动端输入焦点回归需要观察 beforeinput 到 App 输入发送之间的状态机事件。
    (globalThis as { __TERMD_TRACE__?: boolean }).__TERMD_TRACE__ = true;
  });
}

async function attachTermdDiagnostics(
  testInfo: { attach: (name: string, options: { body: string; contentType: string }) => Promise<void> },
  label: string,
  page: Page,
): Promise<void> {
  const events = await page
    .evaluate(() => (globalThis as { __TERMD_DIAG_EVENTS__?: unknown[] }).__TERMD_DIAG_EVENTS__ ?? [])
    .catch(() => []);
  if (events.length === 0) {
    return;
  }
  await testInfo.attach(`termd-diagnostics-${label}.json`, {
    body: JSON.stringify(events, null, 2),
    contentType: "application/json",
  });
}

async function terminalViewportText(page: Page): Promise<string> {
  return page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdViewportText ?? "");
}

async function terminalViewportNumberLines(page: Page): Promise<number[]> {
  const text = await terminalViewportText(page);
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => /^\d{4}$/.test(line))
    .map((line) => Number.parseInt(line, 10));
}

async function terminalViewportState(page: Page): Promise<{ viewportRaw: number; scrollbackLength: number }> {
  return page.locator(".terminal-host").evaluate((host) => {
    const element = host as HTMLElement;
    return {
      viewportRaw: Number.parseFloat(element.dataset.termdViewportYRaw ?? ""),
      scrollbackLength: Number.parseFloat(element.dataset.termdScrollbackLength ?? ""),
    };
  });
}

async function terminalHostSize(page: Page): Promise<{ cols: number; rows: number }> {
  return page.locator(".terminal-host").evaluate((host) => {
    const element = host as HTMLElement;
    return {
      cols: Number.parseInt(element.dataset.termdCols ?? "0", 10),
      rows: Number.parseInt(element.dataset.termdRows ?? "0", 10),
    };
  });
}

async function waitForStableTerminalSurface(page: Page): Promise<void> {
  await expect
    .poll(async () => page.locator(".terminal-host").evaluate((host) => {
      const element = host as HTMLElement;
      const surface =
        element.querySelector<HTMLElement>("canvas") ??
        element.querySelector<HTMLElement>(".xterm-screen") ??
        element.querySelector<HTMLElement>(".xterm-viewport") ??
        element.querySelector<HTMLElement>(".xterm");
      if (!surface) {
        return false;
      }
      const rect = surface.getBoundingClientRect();
      return (
        rect.width > 0 &&
        rect.height > 0 &&
        element.dataset.termdResizeStabilizing !== "true" &&
        element.dataset.termdSnapshotRedraw !== "true" &&
        Number.parseInt(element.dataset.termdRows ?? "0", 10) > 0 &&
        Number.parseInt(element.dataset.termdCols ?? "0", 10) > 0
      );
    }), { timeout: 8_000 })
    .toBe(true);
}

async function focusTerminalKeyboardSink(page: Page): Promise<void> {
  const terminalSurface = page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first();
  const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();
  await terminalSurface.click({ position: { x: 20, y: 20 } });
  await expect(terminalInput).toBeFocused();
}

async function terminalSurfaceMetrics(page: Page): Promise<{
  left: number;
  top: number;
  width: number;
  height: number;
  rows: number;
  cols: number;
}> {
  return page.locator(".terminal-host").evaluate((host) => {
    const element = host as HTMLElement;
    const surface =
      element.querySelector<HTMLElement>("canvas") ??
      element.querySelector<HTMLElement>(".xterm-screen") ??
      element.querySelector<HTMLElement>(".xterm-viewport") ??
      element.querySelector<HTMLElement>(".xterm");
    if (!surface) {
      throw new Error("terminal surface is missing");
    }
    const rect = surface.getBoundingClientRect();
    return {
      left: rect.left,
      top: rect.top,
      width: rect.width,
      height: rect.height,
      rows: Number.parseInt(element.dataset.termdRows ?? "0", 10),
      cols: Number.parseInt(element.dataset.termdCols ?? "0", 10),
    };
  });
}

async function expectTerminalScrollAtBottom(page: Page): Promise<void> {
  // 中文注释：外层 scrollport 可能已经“看起来”在底部，但 xterm 视口还没真正
  // 追平。这里把 renderer 视口一起纳入条件，避免继续把旧历史当作当前屏幕。
  await expect
    .poll(async () => {
      const [scrollportPinned, viewportState] = await Promise.all([
        page.locator(".terminal-scrollport").evaluate((element) => {
          const maxScrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
          return element.scrollTop >= maxScrollTop - 2;
        }),
        terminalViewportState(page),
      ]);
      // 中文注释：viewportRaw 表示距底部的原始距离，只允许极小的浮点抖动。
      return scrollportPinned && Number.isFinite(viewportState.viewportRaw) && viewportState.viewportRaw <= 0.5;
    })
    .toBe(true);
}
