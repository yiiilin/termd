import { expect, test, type Locator, type Page } from "@playwright/test";
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
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
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
        `for i in $(seq 1 900); do printf '${marker(name)}-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-ready`, 10_000);
    }

    for (const name of [...createdNames, ...createdNames].reverse()) {
      await openSession(page, name);
    }
    const targetName = createdNames[0];
    await openSession(page, targetName);
    await expectTerminalLine(page, `${marker(targetName)}-ready`, 3_000);

    await runTerminalCommand(page, `printf '${marker(targetName)}-input-ok\\n'`);
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

  page.on("console", (message) => {
    if (message.type() === "error") {
      browserErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => {
    browserErrors.push(error.message);
  });

  try {
    await page.goto(fixture.relayWebUrl);
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

  try {
    await page.goto(fixture.relayWebUrl);
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
    await secondPage.goto(fixture.relayWebUrl);
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
    await page.goto(fixture.relayWebUrl);
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
        `for i in $(seq 1 2400); do printf '${marker(name)}-jitter-bulk-%04d\\n' "$i"; done; printf '${marker(name)}-jitter-ready\\n'`,
      );
      await expectTerminalLine(page, `${marker(name)}-jitter-ready`, 30_000);
    }

    // 中文注释：第二客户端在 session 已存在后接入，覆盖慢链路同步和后续 attach，
    // 不再依赖已移除的桌面刷新按钮。
    await secondPage.goto(fixture.relayWebUrl);
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
    await page.goto(fixture.relayWebUrl);
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
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto(fixture.relayWebUrl);
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
    await page.goto(fixture.relayWebUrl);
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
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto(fixture.relayWebUrl);
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
    // 中文注释：主干重连后 Web 端应靠自动恢复链路重新拿到 session list。
    await expect.poll(async () => sessionNames(page), { timeout: 20_000 }).toContain(name);

    await openSession(page, name);
    await expectTerminalLine(page, `${marker(name)}-reconnect-ready`, 20_000);
    await runTerminalCommand(page, `printf '${marker(name)}-reconnect-input-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-reconnect-input-ok`, 20_000);

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
    await page.goto(fixture.relayWebUrl);
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);

    const secondToken = await fixture.issuePairingToken();
    await secondPage.goto(fixture.relayWebUrl);
    await secondPage.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await secondPage.getByLabel("Pairing token").fill(pairingInviteCode({ ...fixture, token: secondToken }));
    await secondPage.getByRole("button", { name: "Pair" }).click();
    await expect(secondPage.getByLabel("Pairing token")).toBeHidden();
    // 中文注释：第二个不同分辨率客户端在 session 已存在后接入，
    // 初始 session list 必须包含这个会话，不依赖桌面刷新按钮。
    await expect.poll(async () => sessionNames(secondPage), { timeout: 20_000 }).toContain(name);
    await openSession(secondPage, name);

    await runTerminalCommand(
      page,
      `for i in $(seq 1 3600); do printf '${marker(name)}-shared-bulk-%04d\\n' "$i"; sleep 0.002; done; printf '${marker(name)}-shared-ready\\n'`,
    );
    await expectTerminalLineMatching(page, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-0[0-9]{3}`), 20_000);
    await expectTerminalLineMatching(secondPage, new RegExp(`${escapeRegex(marker(name))}-shared-bulk-0[0-9]{3}`), 20_000);

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
    await expectTerminalLine(page, `${marker(name)}-shared-ready`, 40_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-ready`, 40_000);

    await runTerminalCommand(page, `printf '${marker(name)}-shared-client-a-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-shared-client-a-ok`, 20_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-client-a-ok`, 20_000);
    await runTerminalCommand(secondPage, `printf '${marker(name)}-shared-client-b-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-shared-client-b-ok`, 20_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-client-b-ok`, 20_000);

    // 中文注释：两个不同分辨率客户端必须能同时向同一个 session 写入，
    // 且这类并发输入不应把 websocket / relay 链路打断。
    // 这里不再强求并发 shell 命令的精确整行输出，因为两个客户端同时键入时，
    // shell prompt 本身就可能把字节交错到同一行；我们只验证连接继续可用。
    await Promise.all([
      runTerminalCommand(page, `printf '${marker(name)}-shared-concurrent-client-a\\n'`),
      runTerminalCommand(secondPage, `printf '${marker(name)}-shared-concurrent-client-b\\n'`),
    ]);

    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    await expect(secondPage.getByRole("alert", { name: "Connection error" })).toHaveCount(0);
    await runTerminalCommand(page, `printf '${marker(name)}-shared-post-concurrent-ok\\n'`);
    await expectTerminalLine(page, `${marker(name)}-shared-post-concurrent-ok`, 20_000);
    await expectTerminalLine(secondPage, `${marker(name)}-shared-post-concurrent-ok`, 20_000);
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
    await page.goto(fixture.relayWebUrl);
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
  test.setTimeout(90_000);
  const fixture = await startRealRelayFixture();
  const createdNames: string[] = [];
  const browserErrors: string[] = [];
  collectBrowserErrors(page, "client", browserErrors);

  try {
    await page.goto(fixture.relayWebUrl);
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText("No sessions")).toBeVisible();

    const name = await createShellSession(page, createdNames);
    createdNames.push(name);
    await runTerminalCommand(page, `printf '${marker(name)}-idle-ready\\n'`);
    await expectTerminalLine(page, `${marker(name)}-idle-ready`, 8_000);

    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("blur"));
    });
    // 中文注释：覆盖真实使用中“页面放一阵子再回来”的路径。
    // 12 秒同时跨过前端 10 秒长失焦重建阈值和 relay/daemon idle ping 间隔。
    await sleep(12_000);
    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("focus"));
    });

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
    version: 1,
    ws_url: fixture.relayClientUrl,
    token: fixture.token,
    server_id: fixture.serverId,
    daemon_public_key: fixture.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}

function terminalPane(page: Page): Locator {
  return page.getByTestId("terminal-pane");
}

function collectBrowserErrors(page: Page, label: string, browserErrors: string[]): void {
  page.on("console", (message) => {
    if (message.type() === "error") {
      browserErrors.push(`[${label}:console] ${message.text()}`);
    }
  });
  page.on("pageerror", (error) => {
    browserErrors.push(`[${label}:pageerror] ${error.message}`);
  });
}

async function expectTerminalLine(page: Page, text: string, timeout: number): Promise<void> {
  // xterm 会同时显示命令回显和命令输出；压力测试只关心最终输出行，
  // 这里必须用精确文本，避免 strict locator 被命令回显里的子串干扰。
  await expect(terminalPane(page).getByText(text, { exact: true })).toBeVisible({ timeout });
}

async function expectTerminalLineMatching(page: Page, pattern: RegExp, timeout: number): Promise<void> {
  // 中文注释：持续输出期间具体行号会受网络和重连时机影响；正则只用于确认终端流仍在推进。
  await expect(terminalPane(page).getByText(pattern).first()).toBeVisible({ timeout });
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
  return page
    .getByRole("region", { name: "sessions" })
    .locator(".session-row strong")
    .allTextContents();
}

async function openSession(page: Page, name: string): Promise<void> {
  await page.getByRole("button", { name: `Open ${name}` }).click();
}

async function runTerminalCommand(page: Page, command: string): Promise<void> {
  await terminalPane(page).click();
  const input = page.getByRole("textbox", { name: "Terminal input" });
  await expect(input).toBeAttached({ timeout: 8_000 });
  await input.focus();
  await page.keyboard.insertText(command);
  await page.keyboard.press("Enter");
}

async function closeCreatedSessions(page: Page, names: string[]): Promise<void> {
  for (const name of [...names].reverse()) {
    const row = page.getByRole("button", { name: new RegExp(`^Open ${escapeRegex(name)}(?:, new output)?$`) });
    if (await row.count() === 0) {
      continue;
    }
    await row.getByRole("button", { name: "Close session" }).click();
  }
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
