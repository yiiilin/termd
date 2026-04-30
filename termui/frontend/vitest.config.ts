import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/vitest.setup.ts"],
    exclude: ["tests/**", "node_modules/**", "dist/**"],
    testTimeout: 12000,
  },
});
