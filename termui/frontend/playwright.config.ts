import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  expect: {
    timeout: 8000,
  },
  use: {
    baseURL: "http://127.0.0.1:4173",
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
  ],
  webServer: {
    command: "VITE_TERMD_E2E_DEBUG_BUFFER=1 npm run build && npm run preview",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: false,
    // 中文注释：当前生产构建包含 Monaco worker 与 xterm 资源，冷构建可能超过 2 分钟。
    // E2E 不应该在页面尚未启动前就被 Playwright 判成失败。
    timeout: 300000,
  },
});
