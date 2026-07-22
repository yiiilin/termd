import { expect, test } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

test("desktop CPU chart expands only when the full status row has room", async ({ page }, testInfo) => {
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
    for (const [viewportWidth, expectedChartWidth] of [
      [1181, 30],
      [1279, 30],
      [1280, 56],
      [1440, 56],
    ] as const) {
      await page.setViewportSize({ width: viewportWidth, height: 800 });
      const chart = page.locator(".daemon-cpu-bar-chart");
      await expect.poll(() => chart.evaluate((element) => element.getBoundingClientRect().width)).toBe(expectedChartWidth);

      const layout = await chart.evaluate((element) => {
        const status = element.closest(".daemon-status-strip");
        const grid = element.closest(".daemon-status-grid");
        const frame = element.querySelector(".daemon-cpu-bar-frame");
        if (!status || !grid || !frame) {
          throw new Error("CPU chart must remain inside the daemon status grid with a frame");
        }
        const statusRect = status.getBoundingClientRect();
        const chartRect = element.getBoundingClientRect();
        const frameRect = frame.getBoundingClientRect();
        const values = [...grid.querySelectorAll<HTMLElement>(".daemon-status-metric")]
          .filter((metric) => getComputedStyle(metric).display !== "none")
          .map((metric) => {
            const value = metric.querySelector<HTMLElement>("strong");
            return value ? { clientWidth: value.clientWidth, scrollWidth: value.scrollWidth } : null;
          })
          .filter((value): value is { clientWidth: number; scrollWidth: number } => value !== null);
        return {
          statusHeight: statusRect.height,
          chartHeight: chartRect.height,
          frameHeight: frameRect.height,
          topInset: chartRect.top - statusRect.top,
          bottomInset: statusRect.bottom - chartRect.bottom,
          gridClientWidth: grid.clientWidth,
          gridScrollWidth: grid.scrollWidth,
          values,
        };
      });

      expect(layout.statusHeight).toBe(26);
      expect(layout.chartHeight).toBe(20);
      expect(layout.frameHeight).toBeGreaterThanOrEqual(18);
      expect(Math.abs(layout.topInset - layout.bottomInset)).toBeLessThanOrEqual(1);
      expect(layout.gridScrollWidth).toBeLessThanOrEqual(layout.gridClientWidth);
      expect(layout.values.every((value) => value.scrollWidth <= value.clientWidth)).toBe(true);
    }
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
