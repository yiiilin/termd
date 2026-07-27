import { expect, test } from "@playwright/test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { pairingInviteCode, startRealRelayFixture } from "./real-relay-fixture";

const LARGE_FILE_BYTES = 8 * 1024 * 1024 + 37;

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    window.localStorage.clear();
    window.sessionStorage.clear();
    indexedDB.deleteDatabase("termd-termui-web");
  });
});

test("真实 relay 下 File Offer 提示通过原生导航下载大文件", async ({ page }, testInfo) => {
  test.setTimeout(90_000);
  const offeredDir = await mkdtemp(path.join(tmpdir(), "termd-file-offer-relay-"));
  const fileName = "agent-report.bin";
  const filePath = path.join(offeredDir, fileName);
  const contents = Buffer.alloc(LARGE_FILE_BYTES, "termd file offer through real relay\n");
  await writeFile(filePath, contents, { mode: 0o600 });
  const fixture = await startRealRelayFixture();
  const failedOfferRequests: string[] = [];
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("response", (response) => {
    if (response.url().includes("/api/files/offer") && response.status() >= 400) {
      failedOfferRequests.push(`${response.status()} ${response.url()}`);
    }
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText(/^No sessions?$/).first()).toBeVisible();
    await expect(page.locator(".toolbar-title-refreshing")).toHaveCount(0);
    const workspaceBodyBeforeOffer = await page.locator(".workspace-body").boundingBox();
    expect(workspaceBodyBeforeOffer).not.toBeNull();

    const offer = await fixture.offerFile(filePath);
    await expect(page.getByText(fileName, { exact: true })).toBeVisible();
    await expect(page.getByText(offer.path, { exact: true })).toBeVisible();
    const [workspaceBodyAfterOffer, toolbarBox, offerCenterBox, offerCenterPosition] = await Promise.all([
      page.locator(".workspace-body").boundingBox(),
      page.locator(".toolbar").boundingBox(),
      page.locator(".file-offer-center").boundingBox(),
      page.locator(".file-offer-center").evaluate((element) => getComputedStyle(element).position),
    ]);
    expect(workspaceBodyAfterOffer).toEqual(workspaceBodyBeforeOffer);
    expect(toolbarBox).not.toBeNull();
    expect(offerCenterBox).not.toBeNull();
    expect(offerCenterPosition).toBe("absolute");
    expect(offerCenterBox!.y).toBeGreaterThanOrEqual(toolbarBox!.y + toolbarBox!.height);

    await testInfo.attach("file-offer-prompt.png", {
      body: await page.screenshot(),
      contentType: "image/png",
    });

    const downloadPromise = page.waitForEvent("download");
    await page.getByRole("button", { name: `Download ${fileName}` }).click();
    const download = await downloadPromise;
    expect(download.suggestedFilename()).toBe(fileName);
    const downloadUrl = new URL(download.url());
    expect(downloadUrl.pathname).toMatch(/^\/api\/files\/offer-downloads\/[0-9a-f-]+$/i);
    expect(downloadUrl.searchParams.get("server_id")).toBe(fixture.serverId);
    expect(downloadUrl.search).not.toContain("token");
    const downloadedPath = await download.path();
    expect(downloadedPath).not.toBeNull();
    await expect.poll(async () => readFile(downloadedPath!)).toEqual(contents);
    await expect(page.getByText(fileName, { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: `Download ${fileName}` })).toBeEnabled();
    await page.getByRole("button", { name: `Dismiss ${fileName}` }).click();
    await expect(page.getByText(fileName, { exact: true })).toHaveCount(0);
    expect(failedOfferRequests).toEqual([]);
    expect(pageErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-relay-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    try {
      await fixture.stop();
    } finally {
      await rm(offeredDir, { recursive: true, force: true });
    }
  }
});

test("真实 daemon 直连下 File Offer 通过原生导航下载", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "直连链路只需桌面 Chromium 覆盖");
  test.setTimeout(90_000);
  const offeredDir = await mkdtemp(path.join(tmpdir(), "termd-file-offer-direct-"));
  const fileName = "direct-report.txt";
  const filePath = path.join(offeredDir, fileName);
  const contents = Buffer.from("termd file offer through direct daemon\n", "utf8");
  await writeFile(filePath, contents, { mode: 0o600 });
  const fixture = await startRealRelayFixture({ enableDaemonWeb: true });
  const failedOfferRequests: string[] = [];
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("response", (response) => {
    if (response.url().includes("/api/files/offer") && response.status() >= 400) {
      failedOfferRequests.push(`${response.status()} ${response.url()}`);
    }
  });

  try {
    await page.goto(fixture.daemonWebUrl);
    await page.getByLabel("WS URL").fill(fixture.daemonClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode({
      ...fixture,
      relayClientUrl: fixture.daemonClientUrl,
    }));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect(page.getByLabel("Pairing token")).toBeHidden();
    await expect(page.getByText(/^No sessions?$/).first()).toBeVisible();
    await expect(page.locator(".toolbar-title-refreshing")).toHaveCount(0);

    const offer = await fixture.offerFile(filePath);
    await expect(page.getByText(fileName, { exact: true })).toBeVisible();
    await expect(page.getByText(offer.path, { exact: true })).toBeVisible();

    const downloadPromise = page.waitForEvent("download");
    await page.getByRole("button", { name: `Download ${fileName}` }).click();
    const download = await downloadPromise;
    expect(download.suggestedFilename()).toBe(fileName);
    const downloadUrl = new URL(download.url());
    expect(downloadUrl.pathname).toMatch(/^\/api\/files\/offer-downloads\/[0-9a-f-]+$/i);
    expect(downloadUrl.host).toBe(new URL(fixture.daemonClientUrl).host);
    expect(downloadUrl.search).not.toContain("token");
    const downloadedPath = await download.path();
    expect(downloadedPath).not.toBeNull();
    await expect.poll(async () => readFile(downloadedPath!)).toEqual(contents);
    await expect(page.getByText(fileName, { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: `Download ${fileName}` })).toBeEnabled();
    await page.getByRole("button", { name: `Dismiss ${fileName}` }).click();
    await expect(page.getByText(fileName, { exact: true })).toHaveCount(0);
    expect(failedOfferRequests).toEqual([]);
    expect(pageErrors).toEqual([]);
  } finally {
    await testInfo.attach("real-direct-fixture.log", {
      body: fixture.diagnostics(),
      contentType: "text/plain",
    });
    try {
      await fixture.stop();
    } finally {
      await rm(offeredDir, { recursive: true, force: true });
    }
  }
});
