import { defineConfig, devices } from "@playwright/test";

const webPortRaw = process.env.TERMD_E2E_WEB_PORT ?? "4173";
const webPort = Number(webPortRaw);
if (!Number.isInteger(webPort) || webPort < 1 || webPort > 65_535) {
  throw new Error(`TERMD_E2E_WEB_PORT must be an integer between 1 and 65535, got ${webPortRaw}`);
}
const webBaseUrl = `http://127.0.0.1:${webPort}`;

export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  expect: {
    timeout: 8000,
  },
  use: {
    baseURL: webBaseUrl,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"], viewport: { width: 1366, height: 768 } },
    },
    {
      name: "mobile-chrome",
      use: { ...devices["Pixel 5"] },
    },
    {
      name: "mobile-iphone-layout",
      testMatch: /mobile-terminal-quick-keys\.spec\.ts/,
      use: { ...devices["iPhone 13"], browserName: "chromium" },
    },
  ],
  webServer: {
    // 中文注释：真实 relay fixture 会在测试内执行 cargo run。先在 WebServer 启动阶段
    // 构建嵌入最新 dist 的二进制，避免 CI 冷增量编译占用单测 60 秒超时。
    command:
      "VITE_TERMD_E2E_DEBUG_BUFFER=1 npm run build && " +
      "cargo build --locked --manifest-path ../../Cargo.toml -p termd -p termrelay && " +
      `npx vite preview --host 127.0.0.1 --port ${webPort} --strictPort`,
    url: webBaseUrl,
    reuseExistingServer: false,
    // 中文注释：当前生产构建包含 Monaco worker 与 xterm 资源，冷构建可能超过 2 分钟。
    // E2E 不应该在页面尚未启动前就被 Playwright 判成失败。
    timeout: 300000,
  },
});
