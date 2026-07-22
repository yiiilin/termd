import { expect, test } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

test("desktop CPU status keeps the maximum percentage and chart fully visible", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "desktop layout only needs the Chromium project");

  const daemon = await MockDaemon.start({
    token: "status-layout-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-00000000c100",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "status-layout-ready\n",
    daemonStatus: {
      host_name: "status-host",
      load_avg: [0.12, 0.08, 0.03],
      uptime_seconds: 3600,
      cpu_percent: 100,
      memory_total_bytes: 8 * 1024 * 1024 * 1024,
      memory_available_bytes: 5 * 1024 * 1024 * 1024,
      disk_total_bytes: 128 * 1024 * 1024 * 1024,
      disk_available_bytes: 64 * 1024 * 1024 * 1024,
      network_rx_bytes: 24 * 1024 * 1024,
      network_tx_bytes: 6 * 1024 * 1024,
      process_count: 123,
      atop_available: false,
    },
  });

  try {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/", { waitUntil: "networkidle" });
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await page.getByRole("button", { name: "Pair" }).click();

    const cpuValue = page.locator(".daemon-status-cpu strong");
    await expect(cpuValue).toHaveText("100.0%");
    const dimensions = await cpuValue.evaluate((element) => ({
      clientWidth: element.clientWidth,
      scrollWidth: element.scrollWidth,
    }));

    expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth);

    const chartLayout = await page.locator(".daemon-cpu-bar-chart").evaluate((chart) => {
      const status = chart.closest(".daemon-status-strip");
      const frame = chart.querySelector(".daemon-cpu-bar-frame");
      if (!status || !frame) {
        throw new Error("CPU chart must remain inside the daemon status strip with a frame");
      }
      const statusRect = status.getBoundingClientRect();
      const chartRect = chart.getBoundingClientRect();
      const frameRect = frame.getBoundingClientRect();
      return {
        statusHeight: statusRect.height,
        chartHeight: chartRect.height,
        frameHeight: frameRect.height,
        topInset: chartRect.top - statusRect.top,
        bottomInset: statusRect.bottom - chartRect.bottom,
      };
    });

    expect(chartLayout.statusHeight).toBe(26);
    expect(chartLayout.chartHeight).toBe(20);
    expect(chartLayout.frameHeight).toBeGreaterThanOrEqual(18);
    expect(Math.abs(chartLayout.topInset - chartLayout.bottomInset)).toBeLessThanOrEqual(1);
  } finally {
    await daemon.stop();
  }
});

function pairingInviteCode(daemon: MockDaemon): string {
  const invite = {
    type: "termd_pairing_qr",
    version: 2,
    token: "status-layout-token",
    server_id: daemon.serverId,
    daemon_public_key: daemon.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  };
  return `termd-pair:v2:${Buffer.from(JSON.stringify(invite)).toString("base64url")}`;
}
