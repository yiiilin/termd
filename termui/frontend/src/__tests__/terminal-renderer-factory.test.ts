import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

describe("terminal renderer factory", () => {
  it("默认只创建 Ghostty renderer，不再保留旧 renderer 分支或 fallback", async () => {
    vi.resetModules();
    const rendererSource = readFileSync(resolve(process.cwd(), "src/components/terminal/renderer.ts"), "utf8");
    const { createTerminalRendererInstance } = await import("../components/terminal/renderer");
    const legacyRendererIdentifier = ["create", "X", "term", "Renderer"].join("");
    const legacyPackagePrefix = ["@", "x", "term", "/"].join("");

    const renderer = await createTerminalRendererInstance({
      terminalOptions: {},
      searchOptions: {},
    });

    expect(renderer.kind).toBe("ghostty");
    expect(rendererSource).not.toContain(legacyRendererIdentifier);
    expect(rendererSource).not.toContain(legacyPackagePrefix);
  });
});
