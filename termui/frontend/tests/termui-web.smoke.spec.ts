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
    await expectTerminalLine(page, "termd-e2e-ready", 8_000);

    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toHaveCount(0);
      const sessionListRequests = () =>
        daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list").length;
      const beforeTitlePull = sessionListRequests();
      const titleButton = page.getByRole("button", { name: "Open session list from title" });
      // 中文注释：移动端标题栏下拉刷新复用 session.list，不打开 session 面板。
      // 这里使用 touch pointer 事件覆盖真实浏览器的手势分支。
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
      await expect.poll(sessionListRequests).toBeGreaterThan(beforeTitlePull);
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeHidden();

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
      await expect(daemonStatus.getByText("CPU", { exact: true })).toBeVisible();
      await expect(daemonStatus.getByRole("button", { name: "Refresh server status" })).toHaveCount(0);
    }
    const sessionsPanel = page.getByRole("region", { name: "sessions" });
    // session UUID 已从 UI 隐藏；测试按用户实际看到的可访问名称打开会话。
    const sessionRow = sessionsPanel.getByRole("button", { name: "Open Lagrange" });
    await expect(sessionRow).toBeVisible();

    await sessionRow.click();
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
    expect(daemon.outerWireText()).not.toContain("secret-token");

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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    await page.keyboard.type("direct-after-timeout");
    await page.keyboard.press("Enter");
    await expect.poll(() => daemon.decryptedInputs.join("")).toContain("direct-after-timeout");
    expect(daemon.activeConnectionCount()).toBe(1);
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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    await page.keyboard.type("direct-switch-input-ok");
    await page.keyboard.press("Enter");
    await expect.poll(() => daemon.decryptedInputs.join("")).toContain("direct-switch-input-ok");
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
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
    await page.getByTestId("terminal-pane").hover();
    await page.mouse.wheel(0, -1400);
    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 8_000 })
      .toBeGreaterThan(0);

    const viewportLines = await terminalViewportNumberLines(page);
    expect(viewportLines.length).toBeGreaterThan(10);
    for (let index = 1; index < viewportLines.length; index += 1) {
      // 中文注释：用户反馈 “从 1 打印到 1000 后，上滚看到的数字顺序乱掉”；
      // 当前 viewport 必须仍是逐行递增的历史内容，不能出现 canvas 旧行残留。
      expect(viewportLines[index]).toBe(viewportLines[index - 1] + 1);
    }

    const metrics = await page.locator(".terminal-host canvas").evaluate((canvas) => {
      const rect = (canvas as HTMLCanvasElement).getBoundingClientRect();
      const host = canvas.parentElement as HTMLElement;
      return {
        left: rect.left,
        top: rect.top,
        width: rect.width,
        height: rect.height,
        rows: Number.parseInt(host.dataset.termdRows ?? "0", 10),
        cols: Number.parseInt(host.dataset.termdCols ?? "0", 10),
      };
    });
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

    const metrics = await page.locator(".terminal-host canvas").evaluate((canvas) => {
      const rect = (canvas as HTMLCanvasElement).getBoundingClientRect();
      const host = canvas.parentElement as HTMLElement;
      return {
        left: rect.left,
        top: rect.top,
        width: rect.width,
        height: rect.height,
        rows: Number.parseInt(host.dataset.termdRows ?? "0", 10),
        cols: Number.parseInt(host.dataset.termdCols ?? "0", 10),
      };
    });
    expect(metrics.rows).toBeGreaterThan(5);
    expect(metrics.cols).toBeGreaterThan(10);

    const targetRow = Math.min(metrics.rows - 4, Math.max(6, Math.floor(metrics.rows * 0.6)));
    const expectedLine = (await terminalViewportText(page)).split("\n")[targetRow]?.trim() ?? "";
    expect(expectedLine).toMatch(/^copy-\d{3}$/);
    const selectedLine = await page.evaluate(
      ({ row, endCol }) => {
        const scope = window as typeof window & {
          __TERMD_DEBUG_GHOSTTY__?: {
            selectViewportRange: (
              start: { col: number; row: number },
              end: { col: number; row: number },
            ) => string | undefined;
          };
        };
        return scope.__TERMD_DEBUG_GHOSTTY__?.selectViewportRange(
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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();

    await expect
      .poll(() => daemon.sessionResizes.length, { timeout: 8_000 })
      .toBeGreaterThan(0);
    const initialResizeCount = daemon.sessionResizes.length;

    // 中文注释：用户反馈刷新时肉眼能看到分辨率跳很多次；这里记录真实浏览器
    // 内 Ghostty 的 rows/cols 变化，同时断言 shared PTY 只收到最终稳定尺寸。
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
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    await page.waitForTimeout(500);

    const reloadResizes = daemon.sessionResizes.slice(initialResizeCount);
    const uniqueReloadDaemonSizes = Array.from(
      new Set(reloadResizes.map((entry) => `${entry.size.cols}x${entry.size.rows}`)),
    );
    const ghosttySizeSequence = await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdReloadTerminalSizes?: string[];
      };
      return scope.__termdReloadTerminalSizes ?? [];
    });

    expect(ghosttySizeSequence.length).toBeGreaterThan(0);
    expect(uniqueReloadDaemonSizes.length).toBeLessThanOrEqual(1);
    if (uniqueReloadDaemonSizes.length === 1) {
      const finalGhosttySize = ghosttySizeSequence.at(-1);
      expect(uniqueReloadDaemonSizes[0]).toBe(finalGhosttySize);
    }
    expect(ghosttySizeSequence.length).toBeLessThanOrEqual(2);
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

async function openSession(page: Page, name: string): Promise<void> {
  await page.getByRole("button", { name: `Open ${name}` }).click();
}

async function expectTerminalLine(page: Page, text: string, timeout: number): Promise<void> {
  // 中文注释：Ghostty 只把终端文本画进 canvas；E2E build 显式开启安全的
  // data-termd-buffer 镜像，供浏览器测试验证终端内容。
  await expect
    .poll(async () => terminalDebugBufferText(page), { timeout })
    .toContain(text);
}

async function terminalDebugBufferText(page: Page): Promise<string> {
  return page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? "");
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
      viewportRaw: Number.parseFloat(element.dataset.termdViewportYRaw ?? "0"),
      scrollbackLength: Number.parseFloat(element.dataset.termdScrollbackLength ?? "0"),
    };
  });
}

async function expectTerminalScrollAtBottom(page: Page): Promise<void> {
  await expect
    .poll(async () =>
      page.locator(".terminal-scrollport").evaluate((element) => {
        const maxScrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
        return element.scrollTop >= maxScrollTop - 2;
      }),
    )
    .toBe(true);
}
