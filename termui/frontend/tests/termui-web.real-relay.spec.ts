import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdtemp, open, rm, stat, truncate, writeFile, type FileHandle } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
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

test("真实键盘输入 helper 会等待终端 snapshot/resize 稳定", async ({ page }) => {
  await page.setContent(`
    <div class="terminal-host" data-termd-resize-stabilizing="true">
      <div class="xterm-screen" style="width: 320px; height: 180px"></div>
      <textarea aria-label="Terminal input"></textarea>
    </div>
  `);
  await page.locator(".terminal-host").evaluate((host) => {
    window.setTimeout(() => {
      (host as HTMLElement).dataset.termdResizeStabilizing = "false";
    }, 150);
  });

  await focusTerminalForKeyboard(page);

  expect(await page.locator(".terminal-host").getAttribute("data-termd-resize-stabilizing")).toBe("false");
});

test("浏览器通过真实 relay 连接 daemon 完成 pairing 和 session list", async ({ page }, testInfo) => {
  test.setTimeout(60_000);
  const fixture = await startRealRelayFixture();

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();

    await expect(page.getByLabel("Pairing token")).toBeHidden();
    if (testInfo.project.name === "mobile-chrome") {
      // 中文注释：移动端默认收起 session 列表，首屏只展示 workspace 标题的无会话状态。
      await expect(page.getByText("No session")).toBeVisible();
    } else {
      await expect(page.getByLabel("sessions").getByText("No sessions")).toBeVisible();
    }

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
      await expect(page.getByRole("main", { name: "daemon admin" })).toHaveCount(0);
      await expect(page.getByText("No session")).toBeVisible();

      const reopenedMenu = await openMobileMenu(page);
      await reopenedMenu.getByRole("button", { name: "Sessions" }).click();
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeVisible();
      await activateButton(page, "Refresh sessions");

      // 移动端刷新后，顶部入口仍然必须保持在左侧，不允许被布局规则顶到右边。
      const mobileMenuButton = page
        .locator('button[aria-label="Open mobile workspace menu"], button[aria-label="Close mobile workspace menu"]')
        .first();
      await expect(mobileMenuButton).toBeVisible();
      const menuBox = await mobileMenuButton.boundingBox();
      expect(menuBox?.x ?? 0).toBeLessThan(48);
    }

    await expect(page.getByLabel("sessions").getByText("No sessions")).toBeVisible();

    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain(fixture.token);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await fixture.stop();
  }
});

test("真实 relay 下 clear 之后上滚不会再看到 pre-clear 历史", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "clear/scrollback 回归先覆盖桌面布局");
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];

  try {
    await enableTermdDiagnostics(page);
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(
      page,
      "for i in $(seq 1 80); do printf 'pre-clear-%03d\\n' \"$i\"; done; clear; for i in $(seq 1 120); do printf 'post-clear-%03d\\n' \"$i\"; done; printf 'clear-scroll-%s\\n' ready",
    );
    await expectTerminalLine(page, "clear-scroll-ready", 20_000);
    await resetTermdDiagnostics(page);

    const terminalPane = page.getByTestId("terminal-pane");
    await terminalPane.hover();
    for (let index = 0; index < 10; index += 1) {
      await page.mouse.wheel(0, -1200);
      await page.waitForTimeout(80);
    }

    await expect
      .poll(async () => terminalViewportState(page).then((state) => state.viewportRaw), { timeout: 10_000 })
      .toBeGreaterThan(0);
    await expect
      .poll(async () => terminalDebugBufferText(page), { timeout: 10_000 })
      .toContain("post-clear-001");
    const scrolledViewport = await terminalDebugBufferText(page);
    expect(scrolledViewport).not.toContain("pre-clear-001");
    expect(scrolledViewport).not.toContain("pre-clear-080");
    // 中文注释：这里验证的是“clear 之后只看到 post-clear 历史”，不是强绑某一种
    // scrollback 恢复实现。xterm 本地 scrollback 已足够时，用户向上滚可能不需要
    // 再触发一次 reveal-history full snapshot。
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    const selectedViewportText = await page.evaluate(() => {
      const bridge = (window as typeof window & {
        __TERMD_DEBUG_TERMINAL__?: {
          selectViewportRange: (
            start: { col: number; row: number },
            end: { col: number; row: number },
          ) => string | undefined;
          getSelection: () => string;
        };
      }).__TERMD_DEBUG_TERMINAL__;
      if (!bridge) {
        return "";
      }
      return (
        bridge.selectViewportRange({ col: 0, row: 0 }, { col: 23, row: 5 }) ??
        bridge.getSelection()
      );
    });
    expect(selectedViewportText).toContain("post-clear-001");
    expect(selectedViewportText).not.toContain("pre-clear-001");
    expect(selectedViewportText).not.toContain("pre-clear-080");
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await attachTermdDiagnostics(testInfo, "clear-scroll", page);
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("真实 relay 下多个大输出 session 快速切换后仍能恢复和输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "压力回归只需要桌面布局覆盖真实 relay 链路");
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 3; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalCommand(
        page,
        `for i in $(seq 1 900); do printf '${marker(name)}-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-%s\\n' ready`,
      );
      await expectTerminalLine(page, `${marker(name)}-ready`, 10_000);
    }

    for (const name of [...createdNames, ...createdNames].reverse()) {
      await openSession(page, name);
    }
    const targetName = createdNames[0];
    await openSession(page, targetName);
    await expectTerminalLine(page, `${marker(targetName)}-ready`, 3_000);

    await runTerminalCommand(page, `printf '${marker(targetName)}-%s\\n' input-ok`);
    await expectTerminalLine(page, `${marker(targetName)}-input-ok`, 3_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在 daemon 和 relay 双向 100ms 延迟下多 session 快速切换仍稳定", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "延迟压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture({ daemonToRelayLatencyMs: 100, relayToDaemonLatencyMs: 100 });
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 3; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalCommand(
        page,
        `for i in $(seq 1 1200); do printf '${marker(name)}-latency-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-latency-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-latency-ready`, 15_000);
    }

    for (const name of [...createdNames, ...createdNames, ...createdNames].reverse()) {
      await openSession(page, name);
    }

    const targetName = createdNames[1];
    await openSession(page, targetName);
    await expectTerminalLine(page, `${marker(targetName)}-latency-ready`, 5_000);

    await runTerminalCommand(page, `printf '${marker(targetName)}-latency-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(targetName)}-latency-input-ok`, 5_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在双客户端慢链路下快速切换仍稳定", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "多客户端压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(120_000);
  const fixture = await startRealRelayFixture({
    daemonToRelayLatencyMs: 100,
    relayToDaemonLatencyMs: 100,
    daemonToRelayBytesPerSecond: 96 * 1024,
    relayToDaemonBytesPerSecond: 96 * 1024,
  });
  const secondPage = await page.context().newPage();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client-a", browserErrors);
  collectBrowserErrors(secondPage, "client-b", browserErrors);
  await enableTermdDiagnostics(page);
  await enableTermdDiagnostics(secondPage);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 3; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalCommand(
        page,
        `for i in $(seq 1 1800); do printf '${marker(name)}-slow-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-slow-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-slow-ready`, 20_000);
    }

    // 中文注释：桌面侧边栏已去掉手动刷新按钮；测试双客户端链路时让第二客户端
    // 在 session 已存在后接入，避免测试依赖已移除的手动刷新入口。
    await secondPage.goto("/");
    await expect(secondPage.getByRole("button", { name: "New session" })).toBeVisible();
    await expect.poll(async () => sessionNames(secondPage), { timeout: 10_000 }).toHaveLength(createdNames.length);

    for (let round = 0; round < 8; round += 1) {
      const leftName = createdNames[round % createdNames.length];
      const rightName = createdNames[(round + 1) % createdNames.length];
      await Promise.all([openSession(page, leftName), openSession(secondPage, rightName)]);
    }

    const leftTarget = createdNames[0];
    const rightTarget = createdNames[2];
    await openSession(page, leftTarget);
    await expectTerminalLine(page, `${marker(leftTarget)}-slow-ready`, 8_000);
    await openSession(secondPage, rightTarget);
    await expectTerminalLine(secondPage, `${marker(rightTarget)}-slow-ready`, 8_000);

    await runTerminalCommand(page, `printf '${marker(leftTarget)}-slow-client-a-ok\\n'`);
    await expectTerminalLine(page, `${marker(leftTarget)}-slow-client-a-ok`, 8_000);
    await runTerminalCommand(secondPage, `printf '${marker(rightTarget)}-slow-client-b-ok\\n'`);
    await expectTerminalLine(secondPage, `${marker(rightTarget)}-slow-client-b-ok`, 8_000);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    await expect(secondPage.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await secondPage.close();
    await fixture.stop();
  }
});

test("relay Web 在双客户端抖动低带宽链路下仍能恢复", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "抖动压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(120_000);
  const fixture = await startRealRelayFixture({
    daemonToRelayLatencyMs: 100,
    relayToDaemonLatencyMs: 100,
    daemonToRelayJitterMs: 150,
    relayToDaemonJitterMs: 150,
    daemonToRelayBytesPerSecond: 48 * 1024,
    relayToDaemonBytesPerSecond: 48 * 1024,
  });
  const secondPage = await page.context().newPage();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client-a", browserErrors);
  collectBrowserErrors(secondPage, "client-b", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 2; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalPastedCommand(
        page,
        `for i in $(seq 1 2400); do printf '${marker(name)}-jitter-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-jitter-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-jitter-ready`, 30_000);
    }

    // 中文注释：第二客户端在 session 已存在后接入，覆盖慢链路同步和后续 attach，
    // 不再依赖已移除的桌面刷新按钮。
    await secondPage.goto("/");
    await expect(secondPage.getByRole("button", { name: "New session" })).toBeVisible();
    await expect.poll(async () => sessionNames(secondPage), { timeout: 20_000 }).toHaveLength(createdNames.length);

    for (let round = 0; round < 6; round += 1) {
      await Promise.all([
        openSession(page, createdNames[round % createdNames.length]),
        openSession(secondPage, createdNames[(round + 1) % createdNames.length]),
      ]);
    }

    await runTerminalCommand(page, `printf '${marker(createdNames[0])}-jitter-client-a-ok\\n'`);
    await expectTerminalLine(page, `${marker(createdNames[0])}-jitter-client-a-ok`, 10_000);
    await runTerminalCommand(secondPage, `printf '${marker(createdNames[1])}-jitter-client-b-ok\\n'`);
    await expectTerminalLine(secondPage, `${marker(createdNames[1])}-jitter-client-b-ok`, 10_000);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    await expect(secondPage.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await secondPage.close();
    await fixture.stop();
  }
});

test("relay Web 在多个持续输出 session 中快速切换后仍能收尾和输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "持续输出压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(150_000);
  const fixture = await startRealRelayFixture({
    daemonToRelayLatencyMs: 100,
    relayToDaemonLatencyMs: 100,
    daemonToRelayBytesPerSecond: 96 * 1024,
    relayToDaemonBytesPerSecond: 96 * 1024,
  });
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 3; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalCommand(
        page,
        `for i in $(seq 1 2600); do printf '${marker(name)}-live-bulk-%04d\\n' "$i"; sleep 0.001; done; printf '${marker(name)}-live-ready\\n'`,
      );
      await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-live-bulk-0[0-9]{3}`), 20_000);
    }

    // 中文注释：这里不等待任何一个 session 输出完成，模拟用户点到仍在刷屏的终端。
    // relay/daemon 必须清掉旧 attach watcher，并继续推进新 session 的 snapshot/tail。
    for (let round = 0; round < 12; round += 1) {
      await openSession(page, createdNames[round % createdNames.length]);
    }

    for (const name of createdNames) {
      await openSession(page, name);
      await expectTerminalLine(page, `${marker(name)}-live-ready`, 45_000);
    }

    const targetName = createdNames[1];
    await openSession(page, targetName);
    await runTerminalCommand(page, `printf '${marker(targetName)}-live-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(targetName)}-live-input-ok`, 10_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在两个大输出 session 中每 0.5 秒切换 20 次后仍能恢复输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "持续输出压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(150_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors, {
    ignoreExpectedRelayInterruptHttpErrors: true,
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 2; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalCommand(
        page,
        `for i in $(seq 1 6000); do printf '${marker(name)}-toggle-bulk-%04d\\n' "$i"; sleep 0.001; done; printf '${marker(name)}-toggle-ready\\n'`,
      );
      await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-toggle-bulk-0[0-9]{3}`), 20_000);
    }

    // 中文注释：精确覆盖用户复现动作：两个正在大量输出的终端，每 0.5 秒来回切换 20 次。
    // 旧 stream 的已排队输出必须被丢弃，不能把最后停住的 session 卡在旧输出后面。
    for (let round = 0; round < 20; round += 1) {
      await openSession(page, createdNames[round % createdNames.length]);
      await sleep(500);
    }

    const targetName = createdNames[0];
    await openSession(page, targetName);
    await expectTerminalLine(page, `${marker(targetName)}-toggle-ready`, 20_000);
    await expectTerminalScrollAtBottom(page);
    await runTerminalCommand(page, `printf '${marker(targetName)}-toggle-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(targetName)}-toggle-input-ok`, 8_000);
    await expectTerminalScrollAtBottom(page);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在 daemon relay 短暂冻结恢复后仍能输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "冻结恢复压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture({
    daemonToRelayLatencyMs: 100,
    relayToDaemonLatencyMs: 100,
    blackoutAfterMs: 4_000,
    blackoutDurationMs: 3_000,
  });
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await enableTermdDiagnostics(page);
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    for (let index = 0; index < 2; index += 1) {
      const name = await createShellSession(page, createdNames);
      createdNames.push(name);
      await runTerminalPastedCommand(
        page,
        `for i in $(seq 1 1600); do printf '${marker(name)}-freeze-bulk-%04d\\n' "$i"; sleep 0.001; done; printf '${marker(name)}-freeze-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-freeze-ready`, 25_000);
    }

    for (const name of [...createdNames, ...createdNames].reverse()) {
      await openSession(page, name);
    }
    const targetName = createdNames[0];
    await openSession(page, targetName);
    await expectTerminalLine(page, `${marker(targetName)}-freeze-ready`, 8_000);
    await runTerminalCommand(page, `printf '${marker(targetName)}-freeze-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(targetName)}-freeze-input-ok`, 8_000);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await attachTermdDiagnostics(testInfo, "freeze-recovery", page);
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 放大终端后 xterm 终端表面和输入仍可用", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "桌面回归即可覆盖 xterm resize 后的输入路径");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];

  try {
    await page.setViewportSize({ width: 1366, height: 420 });
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(
      page,
      `for i in $(seq 1 100); do printf '${marker(name)}-anchor-%03d\\n' "$i"; done; printf '${marker(name)}-anchor-ready\\n'`,
    );
    await expectTerminalLine(page, `${marker(name)}-anchor-ready`, 20_000);

    const beforeResize = await terminalCanvasMetrics(page);
    expect(beforeResize.canvasCssHeight).toBeGreaterThan(0);
    expect(beforeResize.canvasPixelHeight).toBeGreaterThan(0);
    expect(beforeResize.inputAttached).toBe(true);
    await expectTerminalCanvasPainted(page);

    await page.setViewportSize({ width: 1366, height: 960 });
    await expect
      .poll(async () => (await terminalCanvasMetrics(page)).canvasCssHeight, { timeout: 20_000 })
      .toBeGreaterThan(beforeResize.canvasCssHeight + 200);
    await waitForTerminalCanvasStable(page);
    await expect(page.getByRole("textbox", { name: "Terminal input" })).toBeAttached({ timeout: 8_000 });
    await runTerminalCommand(page, `printf '${marker(name)}-anchor-post-resize\\n'`);
    await expectTerminalLine(page, `${marker(name)}-anchor-post-resize`, 10_000);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 短输出放大后 xterm 输入仍落入当前 session", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "桌面回归即可覆盖 xterm resize 边界");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];

  try {
    await page.setViewportSize({ width: 1366, height: 420 });
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    const smallViewport = await terminalCanvasMetrics(page);
    const lineCount = Math.max(10, Math.floor(smallViewport.canvasCssHeight / 18) + 6);
    await runTerminalCommand(
      page,
      `for i in $(seq 1 ${lineCount}); do printf '${marker(name)}-short-%02d\\n' "$i"; done; printf '${marker(name)}-short-ready\\n'`,
    );
    await expectTerminalLine(page, `${marker(name)}-short-ready`, 20_000);

    const beforeResize = await terminalCanvasMetrics(page);
    expect(beforeResize.inputAttached).toBe(true);

    await page.setViewportSize({ width: 1366, height: 960 });
    await expect
      .poll(async () => (await terminalCanvasMetrics(page)).canvasCssHeight, { timeout: 20_000 })
      .toBeGreaterThan(smallViewport.canvasCssHeight + 200);
    await page.getByRole("textbox", { name: "Terminal input" }).focus();

    const afterResize = await terminalCanvasMetrics(page);
    expect(afterResize.inputAttached).toBe(true);
    expect(afterResize.canvasPixelHeight).toBeGreaterThan(beforeResize.canvasPixelHeight);
    await runTerminalCommand(page, `printf '${marker(name)}-short-post-resize\\n'`);
    await expectTerminalLine(page, `${marker(name)}-short-post-resize`, 10_000);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 满屏新会话连续回车后 xterm 仍保持可输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "桌面回归即可覆盖满屏 xterm 输入场景");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];

  try {
    await page.setViewportSize({ width: 1366, height: 960 });
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    const input = page.getByRole("textbox", { name: "Terminal input" });
    await expect(input).toBeAttached({ timeout: 8_000 });
    await input.focus();
    for (let index = 0; index < 100; index += 1) {
      await page.keyboard.press("Enter");
    }
    await runTerminalCommand(page, `printf '${marker(name)}-enter-post-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-enter-post-input-ok`, 20_000);
    const metrics = await terminalCanvasMetrics(page);
    // 中文注释：这是用户手工复现路径：新会话在满屏高度下连续回车后，
    // 不做任何额外重连，直接验证 xterm 终端表面仍在渲染且隐藏输入框仍可接收输入。
    expect(metrics.canvasCssHeight).toBeGreaterThan(500);
    expect(metrics.inputAttached).toBe(true);
    await expectTerminalCanvasPainted(page);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在 daemon relay 主干断开重连后仍能恢复输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "主干重连压力回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(120_000);
  const fixture = await startRealRelayFixture({
    daemonToRelayLatencyMs: 100,
    relayToDaemonLatencyMs: 100,
    enableRelayInterrupt: true,
  });
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors, {
    ignoreExpectedRelayInterruptHttpErrors: true,
  });

  try {
    await enableTermdDiagnostics(page);
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(
      page,
      `for i in $(seq 1 1000); do printf '${marker(name)}-reconnect-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-reconnect-ready\\n'`,
    );
    await expectTerminalLine(page, `${marker(name)}-reconnect-ready`, 15_000);

    await fixture.interruptRelayMux();
    await fixture.waitForRelayReady();
    // 中文注释：主干重连后，当前 attach 的自动恢复可能先于 session list 刷新完成。
    // 这里直接以“恢复后还能继续输入并看到新输出”作为验收边界，不把列表刷新时序当成失败。
    await waitForStableTerminalSurface(page);
    await runTerminalCommand(page, `printf '${marker(name)}-reconnect-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-reconnect-input-ok`, 20_000);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    await attachTermdDiagnostics(testInfo, "relay-mux-reconnect", page);
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 在 daemon 重启后自动恢复当前 session 并继续输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "daemon 重启恢复回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(120_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors, {
    ignoreExpectedRelayInterruptHttpErrors: true,
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(
      page,
      `(for i in $(seq 1 40); do printf '${marker(name)}-daemon-restart-stream-%02d\\n' "$i"; sleep 0.25; done) & printf '${marker(name)}-daemon-restart-ready\\n'`,
    );
    await expectTerminalLine(page, `${marker(name)}-daemon-restart-ready`, 8_000);
    await expectTerminalLine(page, `${marker(name)}-daemon-restart-stream-03`, 8_000);

    await fixture.restartDaemon();
    // 中文注释：daemon 重启后，浏览器必须通过 relay 自动重建 daemon 连接，
    // 重新同步 session list，并对当前选中的 session 自动重新 attach。
    // 这里要求在没有任何后续用户输入的情况下看到 restart 之后才会出现的持续输出，
    // 避免测试只因为重启前遗留 UI 状态而误通过。
    await expect.poll(async () => sessionNames(page), { timeout: 20_000 }).toContain(name);
    await expectTerminalLine(page, `${marker(name)}-daemon-restart-stream-20`, 20_000);

    await runTerminalCommand(page, `printf '${marker(name)}-daemon-restart-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-daemon-restart-input-ok`, 20_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 上传文件时有发送进度并写入当前会话目录", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "文件上传链路用桌面布局覆盖真实 relay");
  const largeUploadBytes = Number(process.env.REAL_RELAY_UPLOAD_BYTES ?? "0");
  const requiredLargeUploadBytes = 300 * 1024 * 1024;
  if (largeUploadBytes > 0 && largeUploadBytes !== requiredLargeUploadBytes) {
    throw new Error(`REAL_RELAY_UPLOAD_BYTES must be ${requiredLargeUploadBytes} for the 300MB relay upload check`);
  }
  test.setTimeout(largeUploadBytes > 0 ? 300_000 : 90_000);
  // 中文注释：文件上传验收必须落在专用 /tmp 子目录。这样既避免列 /tmp 或 /root 时
  // 被测试机已有文件干扰，也让小文件 WebSocket fallback 和 300MB HTTP tunnel 共用同一目标语义。
  const uploadTargetDir = await mkdtemp(path.join(tmpdir(), "termd-relay-upload-target-"));
  const resolvedTmpDir = path.resolve(tmpdir());
  const resolvedUploadTargetDir = path.resolve(uploadTargetDir);
  if (resolvedTmpDir !== "/tmp" || !resolvedUploadTargetDir.startsWith(`${resolvedTmpDir}${path.sep}`)) {
    throw new Error(`relay upload target must be under /tmp, got ${resolvedUploadTargetDir}`);
  }
  const fixture = await startRealRelayFixture(largeUploadBytes > 0
    ? {
      enableHttpTunnel: true,
      daemonEnv: { TERMD_DEFAULT_WORKING_DIRECTORY: uploadTargetDir },
    }
    : {
      daemonEnv: { TERMD_DEFAULT_WORKING_DIRECTORY: uploadTargetDir },
      daemonToRelayLatencyMs: 100,
      relayToDaemonLatencyMs: 100,
      daemonToRelayBytesPerSecond: 64 * 1024,
      relayToDaemonBytesPerSecond: 64 * 1024,
    });
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  let uploadTempDir: string | undefined;
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const fileName = largeUploadBytes > 0 ? "relay-upload-large.bin" : "relay-upload-progress.txt";
    const fileHead = "relay upload progress line";
    const fileTail = "termd-large-upload-tail";
    const content = `${fileHead}\n`.repeat(16384);
    const expectedBytes = largeUploadBytes > 0 ? largeUploadBytes : Buffer.byteLength(content, "utf8");
    const uploadTargetPath = `${uploadTargetDir}/${fileName}`;
    const uploadDirMarker = `termd-upload-dir-${Date.now()}`;
    const largeUploadMarkers = largeUploadBytes > 0
      ? largeUploadMarkerSpecs(largeUploadBytes, fileHead, fileTail)
      : [];

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    const prepareUploadCommand = `cd ${uploadTargetDir} && rm -f ${fileName} ${uploadDirMarker}; : > ${uploadDirMarker}; printf '${marker(name)}-upload-ready\\n'`;
    await runTerminalCommand(page, prepareUploadCommand);
    await expectTerminalLine(page, `${marker(name)}-upload-ready`, 8_000);

    const filesPanel = page.getByLabel("session files");
    await expect(filesPanel).toBeVisible();
    // 中文注释：fixture 通过 daemon 专用环境变量把新 session 默认 cwd 固定到目标目录；
    // marker 确认文件面板确实跟随到该目录，后续上传目标才不会落到根目录或用户家目录。
    await expect(filesPanel.getByLabel("Current directory")).toHaveValue(uploadTargetDir, { timeout: 10_000 });
    await filesPanel.getByRole("button", { name: "Refresh files", exact: true }).click();
    await expect.poll(async () => sessionFileNames(filesPanel), { timeout: 20_000 }).toContain(uploadDirMarker);
    if (largeUploadBytes > 0) {
      uploadTempDir = await mkdtemp(path.join(tmpdir(), "termd-large-upload-"));
      const filePath = path.join(uploadTempDir, fileName);
      await writeSparseLargeUploadFixture(filePath, largeUploadBytes, fileHead, fileTail, largeUploadMarkers);
      await filesPanel.getByLabel("Upload file").setInputFiles(filePath);
    } else {
      await filesPanel.getByLabel("Upload file").setInputFiles({
        name: fileName,
        mimeType: "text/plain",
        buffer: Buffer.from(content, "utf8"),
      });
    }

    await expect(filesPanel.getByRole("status", { name: `Uploading ${fileName}` })).toBeVisible();
    await expect
      .poll(async () => uploadProgressPercentValue(filesPanel), { timeout: 15_000 })
      .toBeGreaterThan(0);
    await expect
      .poll(async () => refreshAndListSessionFileNames(filesPanel), { timeout: largeUploadBytes > 0 ? 180_000 : 30_000 })
      .toContain(fileName);

    if (largeUploadBytes > 0) {
      await verifySparseLargeUploadTarget(uploadTargetPath, largeUploadBytes, fileHead, fileTail, largeUploadMarkers);
    }

    const verifyCommand = largeUploadBytes > 0
      ? `bytes=$(wc -c < ${uploadTargetPath}); printf '${marker(name)}-upload-size:%s\\n' "$bytes"; printf '${marker(name)}-upload-head:'; head -c 26 ${uploadTargetPath}; printf '\\n'; printf '${marker(name)}-upload-tail:'; tail -c ${Buffer.byteLength(fileTail, "utf8")} ${uploadTargetPath}; printf '\\n'; printf '${marker(name)}-upload-markers:%s\\n' '${largeUploadMarkers.length}'; rm -f ${uploadTargetPath} ${uploadTargetDir}/${uploadDirMarker}`
      : `bytes=$(wc -c < ${uploadTargetPath}); printf '${marker(name)}-upload-size:%s\\n' "$bytes"; printf '${marker(name)}-upload-head:'; head -c 26 ${uploadTargetPath}; printf '\\n'; rm -f ${uploadTargetPath} ${uploadTargetDir}/${uploadDirMarker}`;
    await runTerminalCommand(page, verifyCommand);
    await expectTerminalLine(page, `${marker(name)}-upload-size:${expectedBytes}`, largeUploadBytes > 0 ? 60_000 : 20_000);
    await expectTerminalLine(page, `${marker(name)}-upload-head:${fileHead}`, 20_000);
    if (largeUploadBytes > 0) {
      await expectTerminalLine(page, `${marker(name)}-upload-tail:${fileTail}`, 20_000);
      await expectTerminalLine(page, `${marker(name)}-upload-markers:${largeUploadMarkers.length}`, 20_000);
    }
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    // 中文注释：小文件 smoke 继续覆盖“HTTP tunnel 关闭时回退 WebSocket”的兼容路径；
    // 300MB 用例则显式打开 tunnel，确保真正走带发送进度的 HTTP 分片上传。
    expect(browserErrors.filter((entry) => !entry.includes("status of 501 (Not Implemented)"))).toEqual([]);
  } finally {
    if (process.env.REAL_RELAY_PRINT_DIAGNOSTICS === "1") {
      // 中文注释：默认不刷真实 relay 上传日志；单次排查 CI/本地失败时可显式打开。
      console.log(fixture.diagnostics());
    }
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    try {
      await closeCreatedSessions(page, createdNames);
      await fixture.stop();
    } finally {
      if (uploadTempDir) {
        await rm(uploadTempDir, { recursive: true, force: true });
      }
      if (uploadTargetDir) {
        await rm(uploadTargetDir, { recursive: true, force: true });
      }
    }
  }
});

test("relay Web 双客户端同会话不同分辨率轮番离线上线后仍能恢复", async ({ page, browser }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "多客户端断续回归只需要桌面布局覆盖真实 relay 链路");
  test.setTimeout(150_000);

  const fixture = await startRealRelayFixture({ daemonToRelayLatencyMs: 100, relayToDaemonLatencyMs: 100 });
  const secondContext = await browser.newContext({ viewport: { width: 1024, height: 700 } });
  const secondPage = await secondContext.newPage();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client-a", browserErrors);
  collectBrowserErrors(secondPage, "client-b", browserErrors);

  try {
    await page.setViewportSize({ width: 1366, height: 768 });
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);

    const secondToken = await fixture.issuePairingToken();
    await secondPage.goto("/");
    await secondPage.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await secondPage.getByLabel("Pairing token").fill(pairingInviteCode({ ...fixture, token: secondToken }));
    await secondPage.getByRole("button", { name: "Pair" }).click();
    await expect(secondPage.getByLabel("Pairing token")).toBeHidden();
    // 中文注释：第二个不同分辨率客户端在 session 已存在后接入，
    // 初始 session list 必须包含这个会话，不依赖桌面刷新按钮。
    await expect.poll(async () => sessionNames(secondPage), { timeout: 20_000 }).toContain(name);
    await openSession(secondPage, name);
    const bulkDoneFile = `${marker(name)}-shared-done`;

    await runTerminalCommand(
      page,
      `rm -f '${bulkDoneFile}'; for i in $(seq 1 1200); do printf '${marker(name)}-shared-bulk-%04d\\n' "$i"; sleep 0.006; done; printf '${marker(name)}-shared-ready\\n'; : > '${bulkDoneFile}'`,
    );
    await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-0[0-9]{3}`), 20_000);
    await expectTerminalLineMatching(secondPage, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-0[0-9]{3}`), 20_000);
    await resetTermdDiagnostics(page);
    await resetTermdDiagnostics(secondPage);

    // 中文注释：这里用浏览器上下文 offline 模拟两个客户端轮番掉线，daemon 与 relay 主干仍保持在线。
    await page.context().setOffline(true);
    await sleep(1_200);
    await page.context().setOffline(false);
    await expectTerminalLineMatching(secondPage, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-[0-9]{4}`), 20_000);

    await secondContext.setOffline(true);
    await sleep(1_200);
    await secondContext.setOffline(false);
    await openSession(secondPage, name);
    await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-[0-9]{4}`), 20_000);

    await page.context().setOffline(true);
    await sleep(1_000);
    await page.context().setOffline(false);
    await secondContext.setOffline(true);
    await sleep(1_000);
    await secondContext.setOffline(false);

    await openSession(page, name);
    await openSession(secondPage, name);
    await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-[0-9]{4}`), 40_000);
    await expectTerminalLineMatching(secondPage, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-[0-9]{4}`), 40_000);
    // 中文注释：attach snapshot 可能先带出 future prompt，再补旧 bulk tail；单看终端文本
    // 仍然会被回放顺序误导。这里改用文件树确认长命令已经真正跑到末尾：
    // done 文件只会在 shell 完成最后一条 `: > file` 后存在，这是 daemon 侧的硬边界。
    await waitForSessionFile(page, bulkDoneFile, 60_000);
    // 中文注释：文件树能证明 shell 已完成，但前端可能仍在追平大量 terminal tail。
    // 继续输入前必须等可见终端也回放到 ready/prompt 附近，否则按键会落入未完成的续行提示。
    await expectTerminalLine(page, `${marker(name)}-shared-ready`, 40_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-ready`, 40_000);
    await expectTerminalScrollAtBottom(page);
    await expectTerminalScrollAtBottom(secondPage);
    // 中文注释：恢复后仍要证明这两个客户端继续操作的是“同一个共享 session”，
    // 所以这里要求 A、B 两端都能各自执行一条新命令，并且输出会同步出现在对端页面。
    // 中文注释：双客户端轮番 offline/online 后，这里验证的是“恢复后仍在同一个共享 session 上
    // 正常执行新命令并同步到对端”，不是浏览器逐键输入链路本身。GitHub runner 上真实键盘
    // 在 shared tail 仍持续追平时偶发会漏掉命令前缀，导致 bash 落进 `>` 续行提示；
    // 这两条恢复后验收命令改走 paste 注入，保持用例聚焦在 shared-session recovery。
    await runTerminalPastedCommand(page, `printf '${marker(name)}-shared-after-reconnect-client-a\\n'`);
    await expectTerminalLine(page, `${marker(name)}-shared-after-reconnect-client-a`, 40_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-after-reconnect-client-a`, 40_000);
    await expectTerminalScrollAtBottom(page);
    await expectTerminalScrollAtBottom(secondPage);
    await runTerminalPastedCommand(secondPage, `rm -f '${bulkDoneFile}'; printf '${marker(name)}-shared-after-reconnect-client-b\\n'`);
    await expectTerminalLine(page, `${marker(name)}-shared-after-reconnect-client-b`, 40_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-after-reconnect-client-b`, 40_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    await expect(secondPage.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await attachTermdDiagnostics(testInfo, "client-a", page);
    await attachTermdDiagnostics(testInfo, "client-b", secondPage);
    await page.context().setOffline(false).catch(() => undefined);
    await secondContext.setOffline(false).catch(() => undefined);
    await closeCreatedSessions(page, createdNames);
    await secondContext.close();
    await fixture.stop();
  }
});

test("relay Web 后台恢复后重建当前 session 并能继续输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "后台恢复回归用桌面项目覆盖真实 relay 链路");
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(page, `printf '${marker(name)}-background-ready\\n'`);
    await expectTerminalLine(page, `${marker(name)}-background-ready`, 8_000);

    // 中文注释：真实移动/后台浏览器可能让旧 WebSocket 半开；这里模拟 visibility
    // 恢复路径，要求前端重建当前 terminal 连接并重新拿 snapshot。
    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await sleep(200);
    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("focus"));
    });

    await expectTerminalLine(page, `${marker(name)}-background-ready`, 20_000);
    await runTerminalCommand(page, `printf '${marker(name)}-background-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-background-input-ok`, 10_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

test("relay Web 后台空闲超过保活间隔后恢复仍能继续输入", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "长空闲恢复回归用桌面项目覆盖真实 relay 链路");
  test.setTimeout(120_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(page, `printf '${marker(name)}-idle-ready\\n'`);
    await expectTerminalLine(page, `${marker(name)}-idle-ready`, 8_000);

    const cdp = await page.context().newCDPSession(page);
    try {
      await page.evaluate(() => {
        Object.defineProperty(document, "visibilityState", {
          configurable: true,
          get: () => "hidden",
        });
        document.dispatchEvent(new Event("visibilitychange"));
        window.dispatchEvent(new Event("blur"));
      });
      // 中文注释：仅修改 visibilityState 不会暂停 JS，无法复现移动系统冻结 PWA 的行为。
      // 真正冻结 35 秒，跨过 supervisor 的 30 秒 heartbeat timeout，再验证 attach 恢复。
      await cdp.send("Page.setWebLifecycleState", { state: "frozen" });
      await sleep(35_000);
      await cdp.send("Page.setWebLifecycleState", { state: "active" });
      await page.evaluate(() => {
        Object.defineProperty(document, "visibilityState", {
          configurable: true,
          get: () => "visible",
        });
        document.dispatchEvent(new Event("visibilitychange"));
        window.dispatchEvent(new Event("focus"));
      });
    } finally {
      await cdp.detach().catch(() => undefined);
    }

    await expectTerminalLine(page, `${marker(name)}-idle-ready`, 20_000);
    await runTerminalCommand(page, `printf '${marker(name)}-idle-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-idle-input-ok`, 10_000);
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    if (browserErrors.length > 0) {
      await testInfo.attach("browser-errors.log", {
        body: browserErrors.join("\n"),
        contentType: "text/plain",
      });
    }
    await closeCreatedSessions(page, createdNames);
    await fixture.stop();
  }
});

function pairingInviteCode(fixture: { relayClientUrl: string; serverId: string; token: string; daemonPublicKey: string }): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 2,
    ws_url: fixture.relayClientUrl,
    token: fixture.token,
    server_id: fixture.serverId,
    daemon_public_key: fixture.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v2:${Buffer.from(payload, "utf8").toString("base64url")}`;
}

function terminalPane(page: Page): Locator {
  return page.getByTestId("terminal-pane");
}

function collectBrowserErrors(
  page: Page,
  label: string,
  browserErrors: string[],
  options: { ignoreExpectedRelayInterruptHttpErrors?: boolean } = {},
): void {
  page.on("console", (message) => {
    if (message.text().includes("net::ERR_INTERNET_DISCONNECTED")) {
      return;
    }
    if (message.text().includes("net::ERR_NETWORK_CHANGED")) {
      return;
    }
    // 中文注释：只有真实 relay 主干被测试主动切断时，浏览器资源请求可能短暂收到
    // relay 前置层的 502/503/504；其它用例仍应把这些 console error 视为失败。
    if (
      options.ignoreExpectedRelayInterruptHttpErrors &&
      (message.text().includes("status of 502 (Bad Gateway)") ||
        message.text().includes("status of 503 (Service Unavailable)") ||
        message.text().includes("status of 504 (Gateway Timeout)") ||
        message.text().includes("net::ERR_INCOMPLETE_CHUNKED_ENCODING"))
    ) {
      return;
    }
    if (message.type() === "error") {
      browserErrors.push(`[${label}:console] ${message.text()}`);
    }
  });
  page.on("pageerror", (error) => {
    browserErrors.push(`[${label}:pageerror] ${error.message}`);
  });
  page.on("requestfailed", (request) => {
    const failureText = request.failure()?.errorText;
    if (!failureText) {
      return;
    }
    if (failureText.includes("net::ERR_INTERNET_DISCONNECTED")) {
      return;
    }
    if (failureText.includes("net::ERR_NETWORK_CHANGED")) {
      return;
    }
    if (
      options.ignoreExpectedRelayInterruptHttpErrors &&
      failureText.includes("net::ERR_INCOMPLETE_CHUNKED_ENCODING")
    ) {
      return;
    }
    browserErrors.push(`[${label}:requestfailed] ${failureText} ${request.url()}`);
  });
}

async function enableTermdDiagnostics(page: Page): Promise<void> {
  await page.addInitScript(() => {
    // 中文注释：真实 relay 压力回归偶发掉线时，需要保留前端状态机事件用于定位
    // receive loop / reconnect / status refresh 的准确断点。事件仅存在当前测试页面内存里。
    (globalThis as { __TERMD_TRACE__?: boolean }).__TERMD_TRACE__ = true;
  });
}

async function resetTermdDiagnostics(page: Page): Promise<void> {
  await page.evaluate(() => {
    const scope = globalThis as { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: unknown[] };
    scope.__TERMD_TRACE__ = true;
    scope.__TERMD_DIAG_EVENTS__ = [];
  });
}

async function attachTermdDiagnostics(testInfo: { attach: (name: string, options: { body: string; contentType: string }) => Promise<void> }, label: string, page: Page): Promise<void> {
  const events = await page.evaluate(() => (globalThis as { __TERMD_DIAG_EVENTS__?: unknown[] }).__TERMD_DIAG_EVENTS__ ?? []).catch(() => []);
  if (events.length === 0) {
    return;
  }
  await testInfo.attach(`termd-diagnostics-${label}.json`, {
    body: JSON.stringify(events, null, 2),
    contentType: "application/json",
  });
}

interface LargeUploadMarker {
  offset: number;
  text: string;
}

function largeUploadMarkerSpecs(sizeBytes: number, fileHead: string, fileTail: string): LargeUploadMarker[] {
  const markerStepBytes = 1024 * 1024;
  const tailBytes = Buffer.byteLength(fileTail, "utf8");
  const protectedHeadBytes = Buffer.byteLength(`${fileHead}\n`, "utf8");
  const markers: LargeUploadMarker[] = [];
  for (let offset = markerStepBytes; offset + 64 < sizeBytes - tailBytes; offset += markerStepBytes) {
    // 中文注释：每 1MB 放一个非零标记，避免 300MB 稀疏文件只校验头尾时漏掉中间分片丢失。
    markers.push({
      offset: Math.max(offset, protectedHeadBytes + 4096),
      text: `termd-large-upload-marker-${markers.length.toString().padStart(4, "0")}`,
    });
  }
  return markers;
}

async function writeSparseLargeUploadFixture(
  filePath: string,
  sizeBytes: number,
  fileHead: string,
  fileTail: string,
  markers: LargeUploadMarker[],
): Promise<void> {
  // 中文注释：按需大文件回归不把 300MB 内容放进测试源码；文件保持稀疏，
  // 但每个 1MB 区间都有可校验标记，浏览器仍按真实 File 路径上传。
  await writeFile(filePath, Buffer.from(`${fileHead}\n`, "utf8"));
  await truncate(filePath, sizeBytes);
  const handle = await open(filePath, "r+");
  try {
    for (const marker of markers) {
      const markerBytes = Buffer.from(marker.text, "utf8");
      await handle.write(markerBytes, 0, markerBytes.byteLength, marker.offset);
    }
    const tailBytes = Buffer.from(fileTail, "utf8");
    await handle.write(tailBytes, 0, tailBytes.byteLength, Math.max(0, sizeBytes - tailBytes.byteLength));
  } finally {
    await handle.close();
  }
}

async function verifySparseLargeUploadTarget(
  filePath: string,
  sizeBytes: number,
  fileHead: string,
  fileTail: string,
  markers: LargeUploadMarker[],
): Promise<void> {
  const fileInfo = await stat(filePath);
  expect(fileInfo.size).toBe(sizeBytes);
  const handle = await open(filePath, "r");
  try {
    await expectReadUtf8At(handle, 0, fileHead);
    for (const marker of markers) {
      await expectReadUtf8At(handle, marker.offset, marker.text);
    }
    await expectReadUtf8At(handle, sizeBytes - Buffer.byteLength(fileTail, "utf8"), fileTail);
  } finally {
    await handle.close();
  }
}

async function expectReadUtf8At(handle: FileHandle, offset: number, expected: string): Promise<void> {
  const expectedBytes = Buffer.from(expected, "utf8");
  const actual = Buffer.alloc(expectedBytes.byteLength);
  const { bytesRead } = await handle.read(actual, 0, actual.byteLength, offset);
  expect(bytesRead).toBe(expectedBytes.byteLength);
  expect(actual.equals(expectedBytes)).toBe(true);
}

async function expectTerminalLine(page: Page, text: string, timeout: number): Promise<void> {
  await page.bringToFront();
  // 中文注释：xterm 使用 canvas 渲染终端文本，Playwright 不能直接用 DOM
  // 文本定位。只有显式 E2E build 会把当前 buffer 镜像到 data-termd-buffer；
  // 普通 production build 不暴露这个明文快照。
  await expect
    .poll(async () => terminalDebugBufferText(page), { timeout })
    .toContain(text);
}

async function expectTerminalLineMatching(page: Page, pattern: RegExp, timeout: number): Promise<void> {
  await page.bringToFront();
  // 中文注释：持续输出期间具体行号会受网络和重连时机影响；正则只用于确认终端流仍在推进。
  await expect
    .poll(async () => terminalDebugBufferText(page), { timeout })
    .toMatch(pattern);
}

async function expectTerminalScrollAtBottom(page: Page): Promise<void> {
  await page.bringToFront();
  // 中文注释：terminal-scrollport 只是外层容器，真正决定“是否在底部”的是 xterm
  // 视口本身。这里必须同时等外层容器和 renderer 视口都贴底，避免只看 DOM 容器
  // 就误判“已经到底”。
  await expect
    .poll(async () => {
      const [scrollportPinned, viewportState] = await Promise.all([
        page.locator(".terminal-scrollport").evaluate((element) => {
          const maxScrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
          return element.scrollTop >= maxScrollTop - 2;
        }),
        terminalViewportState(page),
      ]);
      // 中文注释：viewportRaw 是“距底部的原始距离”。这里只容忍极小的浮点误差，
      // 不能放过整整一行的偏差。
      return scrollportPinned && Number.isFinite(viewportState.viewportRaw) && viewportState.viewportRaw <= 0.5;
    })
    .toBe(true);
}

async function terminalViewportState(page: Page): Promise<{ viewportRaw: number; scrollbackLength: number }> {
  return page.locator(".terminal-host").evaluate((host) => ({
    viewportRaw: Number.parseFloat((host as HTMLElement).dataset.termdViewportYRaw ?? ""),
    scrollbackLength: Number.parseFloat((host as HTMLElement).dataset.termdScrollbackLength ?? ""),
  }));
}

async function terminalDebugBufferText(page: Page): Promise<string> {
  return page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? "");
}

async function expectTerminalCanvasPainted(page: Page): Promise<void> {
  await expect
    .poll(async () => {
      return page.locator(".terminal-host").evaluate((host) => {
        const typedHost = host as HTMLElement;
        const canvas = typedHost.querySelector<HTMLCanvasElement>("canvas");
        if (!canvas) {
          const surface =
            typedHost.querySelector<HTMLElement>(".xterm-screen") ??
            typedHost.querySelector<HTMLElement>(".xterm-viewport") ??
            typedHost.querySelector<HTMLElement>(".xterm");
          const buffer = typedHost.dataset.termdBuffer ?? "";
          return surface?.getBoundingClientRect().height && buffer.length > 0 ? 1 : 0;
        }
        const context = canvas.getContext("2d");
        if (!context || canvas.width <= 0 || canvas.height <= 0) {
          return 0;
        }
        const sampleWidth = Math.min(canvas.width, 240);
        const sampleHeight = Math.min(canvas.height, 160);
        const pixels = context.getImageData(0, 0, sampleWidth, sampleHeight).data;
        let painted = 0;
        for (let index = 3; index < pixels.length; index += 4) {
          if (pixels[index] !== 0) {
            painted += 1;
          }
        }
        return painted;
      });
    }, { timeout: 20_000 })
    .toBeGreaterThan(0);
}

async function terminalCanvasMetrics(page: Page): Promise<{
  canvasCssHeight: number;
  canvasPixelHeight: number;
  inputAttached: boolean;
}> {
  return page.locator(".terminal-host").evaluate((host) => {
    const typedHost = host as HTMLElement;
    const surface =
      typedHost.querySelector<HTMLElement>("canvas") ??
      typedHost.querySelector<HTMLElement>(".xterm-screen") ??
      typedHost.querySelector<HTMLElement>(".xterm-viewport") ??
      typedHost.querySelector<HTMLElement>(".xterm");
    const canvas = typedHost.querySelector<HTMLCanvasElement>("canvas");
    const input = typedHost.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    if (!surface) {
      throw new Error("terminal surface is missing");
    }
    const canvasRect = surface.getBoundingClientRect();
    return {
      canvasCssHeight: canvasRect.height,
      canvasPixelHeight: canvas?.height ?? Math.round(canvasRect.height * Math.max(1, window.devicePixelRatio || 1)),
      inputAttached: Boolean(input),
    };
  });
}

async function waitForTerminalCanvasStable(page: Page): Promise<void> {
  const deadline = Date.now() + 5_000;
  let lastHeight = -1;
  let stableSamples = 0;
  while (Date.now() < deadline) {
    const { canvasCssHeight } = await terminalCanvasMetrics(page);
    if (Math.abs(canvasCssHeight - lastHeight) < 1) {
      stableSamples += 1;
      if (stableSamples >= 3) {
        return;
      }
    } else {
      lastHeight = canvasCssHeight;
      stableSamples = 0;
    }
    await page.waitForTimeout(100);
  }
  throw new Error("terminal surface did not settle after viewport resize");
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

async function sessionFileNames(filesPanel: Locator): Promise<string[]> {
  return filesPanel.locator(".file-name").allTextContents();
}

async function refreshAndListSessionFileNames(filesPanel: Locator): Promise<string[]> {
  // 中文注释：上传完成后的自动刷新可能和 follow-cwd 静默刷新竞态；E2E 这里显式点击
  // 用户可见的 Refresh，验证文件确实落到当前会话目录，而不是依赖一次性自动刷新时序。
  await filesPanel.getByRole("button", { name: "Refresh files", exact: true }).click();
  return sessionFileNames(filesPanel);
}

async function uploadProgressPercentValue(filesPanel: Locator): Promise<number> {
  return filesPanel.locator(".files-transfer-bar-fill").evaluate((element) => {
    const raw = window.getComputedStyle(element).getPropertyValue("--files-transfer-progress");
    return Number.parseFloat(raw) || 0;
  });
}

async function waitForSessionFile(page: Page, fileName: string, timeout: number): Promise<void> {
  const filesPanel = page.getByLabel("session files");
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    await page.bringToFront();
    await filesPanel.getByRole("button", { name: "Refresh files", exact: true }).click();
    if ((await sessionFileNames(filesPanel)).includes(fileName)) {
      return;
    }
    await page.waitForTimeout(300);
  }
  throw new Error(`session file did not appear: ${fileName}`);
}

async function openSession(page: Page, name: string): Promise<void> {
  await page.getByRole("button", { name: `Open ${name}` }).click();
}

async function runTerminalCommand(page: Page, command: string): Promise<void> {
  await focusTerminalForKeyboard(page);
  // 中文注释：默认继续走真实键盘路径，避免把所有 E2E 都改成非交互输入模型。
  // 只有大段 shell 循环命令才需要专门的稳定注入 helper。
  await page.keyboard.type(command, { delay: 1 });
  await page.keyboard.press("Enter");
}

async function runTerminalPastedCommand(page: Page, command: string): Promise<void> {
  await focusTerminalForKeyboard(page);
  const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();
  await terminalInput.evaluate((input, text) => {
    // 中文注释：GitHub runner 上真实 relay 压测时，`keyboard.type()` 输入超长 shell 循环
    // 命令会偶发漏字，bash 会掉进 `>` 续行提示。这里只给超长批量命令走 paste/beforeinput
    // 注入，保留普通交互命令继续覆盖真实键盘链路。
    input.dispatchEvent(
      new InputEvent("beforeinput", {
        bubbles: true,
        cancelable: true,
        inputType: "insertFromPaste",
        data: `${text}\r`,
      }),
    );
  }, command);
}

async function waitForStableTerminalSurface(page: Page): Promise<void> {
  const deadline = Date.now() + 8_000;
  let lastState:
    | {
        hasSurface: boolean;
        width: number;
        height: number;
        resizeStabilizing: string | undefined;
        snapshotRedraw: string | undefined;
        rows: number;
        cols: number;
        activeTag: string | null;
        activeAriaLabel: string | null;
      }
    | undefined;
  while (Date.now() < deadline) {
    lastState = await page.locator(".terminal-host").evaluate((host) => {
      const element = host as HTMLElement;
      const surface =
        element.querySelector<HTMLElement>("canvas") ??
        element.querySelector<HTMLElement>(".xterm-screen") ??
        element.querySelector<HTMLElement>(".xterm-viewport") ??
        element.querySelector<HTMLElement>(".xterm");
      const rect = surface?.getBoundingClientRect();
      const activeElement = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      return {
        hasSurface: Boolean(surface),
        width: rect?.width ?? 0,
        height: rect?.height ?? 0,
        resizeStabilizing: element.dataset.termdResizeStabilizing,
        snapshotRedraw: element.dataset.termdSnapshotRedraw,
        rows: Number.parseInt(element.dataset.termdRows ?? "0", 10),
        cols: Number.parseInt(element.dataset.termdCols ?? "0", 10),
        activeTag: activeElement?.tagName ?? null,
        activeAriaLabel: activeElement?.getAttribute("aria-label") ?? null,
      };
    });
    if (
      lastState.hasSurface &&
      lastState.width > 0 &&
      lastState.height > 0 &&
      lastState.resizeStabilizing !== "true" &&
      lastState.snapshotRedraw !== "true" &&
      lastState.rows > 0 &&
      lastState.cols > 0
    ) {
      return;
    }
    await page.waitForTimeout(100);
  }
  throw new Error(`terminal surface did not become stable: ${JSON.stringify(lastState)}`);
}

async function focusTerminalForKeyboard(page: Page): Promise<void> {
  const deadline = Date.now() + 8_000;
  const terminalSurface = page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first();
  const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]').first();
  while (Date.now() < deadline) {
    await page.bringToFront();
    if (await terminalInput.count() === 0) {
      await page.waitForTimeout(50);
      continue;
    }
    try {
      // 中文注释：xterm surface 在 snapshot/redraw 窗口可能暂时 hidden；
      // 单次 click 只给一个很短的 budget，超时后回到外层 deadline 继续重试。
      await terminalSurface.click({ position: { x: 20, y: 20 }, timeout: 250 });
    } catch {
      await page.waitForTimeout(50);
      continue;
    }
    const inputReady = await page.locator(".terminal-host").evaluate(async (host) => {
      const input = host.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      if (!input) {
        return false;
      }
      // 中文注释：Chromium 在多页面 bringToFront + reconnect remount 的组合下，
      // helper textarea 可能先短暂 focus，随后又在同一拍掉回 BODY。
      // 这里等待一个小的 settle window，只有稳定留在 activeElement 才允许继续键入。
      input.focus();
      await new Promise((resolve) => window.setTimeout(resolve, 32));
      return (
        document.hasFocus() &&
        document.activeElement === input &&
        host.dataset.termdResizeStabilizing !== "true" &&
        host.dataset.termdSnapshotRedraw !== "true"
      );
    });
    if (inputReady) {
      return;
    }
    await page.waitForTimeout(50);
  }
  throw new Error("terminal input sink did not become active");
}

async function closeCreatedSessions(page: Page, names: string[]): Promise<void> {
  if (page.isClosed()) {
    return;
  }
  for (const name of [...names].reverse()) {
    const deadline = Date.now() + 8_000;
    let lastError: unknown;
    while (Date.now() < deadline) {
      const openButton = page.getByRole("button", {
        name: new RegExp(`^Open ${escapeRegex(name)}(?:, new output)?$`),
      });
      try {
        if ((await openButton.count()) === 0) {
          break;
        }
        // 中文注释：关闭一个 session 会重绘整个列表；每次短重试都从当前 DOM 重新定位 row。
        const row = openButton.locator(
          "xpath=ancestor::*[contains(concat(' ', normalize-space(@class), ' '), ' session-row ')][1]",
        );
        await row.getByRole("button", { name: "Close session" }).click({ timeout: 500 });
      } catch (caught) {
        if (isPageGoneError(caught)) {
          return;
        }
        lastError = caught;
        await page.waitForTimeout(50);
      }
    }
    const remaining = page.getByRole("button", {
      name: new RegExp(`^Open ${escapeRegex(name)}(?:, new output)?$`),
    });
    if ((await remaining.count()) > 0) {
      throw new Error(`failed to close session ${name}`, { cause: lastError });
    }
  }
}

function isPageGoneError(caught: unknown): boolean {
  if (!(caught instanceof Error)) {
    return false;
  }
  return (
    caught.message.includes("Target crashed") ||
    caught.message.includes("Target page, context or browser has been closed") ||
    caught.message.includes("Execution context was destroyed")
  );
}

function marker(name: string): string {
  return `relay-${name.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;
}

function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
