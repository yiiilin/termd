import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("mobile layout regressions", () => {
  it("clamps the terminal pane and xterm host so mobile width cannot overflow the viewport", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain(".toolbar-actions {\n    display: none;");
    expect(css).toContain(".terminal-scrollport {\n    overflow: hidden;");
    expect(css).toContain(".terminal-host {\n    min-width: 0;\n    max-width: 100%;");
    expect(css).toContain(".terminal-host .xterm");
    expect(css).toContain(".terminal-host .xterm .xterm-helper-textarea");
    expect(css).toContain("position: fixed !important;");
    expect(css).toContain("min-height: 0 !important;");
  });
});
