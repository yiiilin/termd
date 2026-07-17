import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

describe("terminal renderer factory", () => {
  it("默认只创建 xterm renderer，不再保留旧 fallback 或双栈分支", async () => {
    vi.resetModules();
    const rendererSource = readFileSync(resolve(process.cwd(), "src/components/terminal/renderer.ts"), "utf8");
    const { createTerminalRendererInstance } = await import("../components/terminal/renderer");
    const removedRendererIdentifier = ["create", "Ghost", "ty", "Renderer"].join("");
    const removedPackageName = ["ghost", "ty", "-", "web"].join("");

    const renderer = await createTerminalRendererInstance({
      terminalOptions: {},
    });

    expect(renderer.kind).toBe("xterm");
    expect(rendererSource).not.toContain(removedRendererIdentifier);
    expect(rendererSource).not.toContain(removedPackageName);
  });
});
