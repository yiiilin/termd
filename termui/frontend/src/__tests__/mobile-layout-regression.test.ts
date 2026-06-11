import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("mobile layout regressions", () => {
  it("clamps the terminal pane and xterm host so mobile width cannot overflow the viewport", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain(".toolbar-actions {\n    display: none;");
    expect(css).toContain(".terminal-scrollport {\n    overflow: hidden;");
    expect(css).toContain("top: var(--termd-visual-viewport-offset-top, 0px);");
    expect(css).toContain(".app-shell.mobile-keyboard-open .workspace {\n    grid-template-rows: 42px minmax(0, 1fr);");
    expect(css).toContain(".app-shell.mobile-keyboard-open .daemon-status-strip {\n    display: none;");
    expect(css).toContain(".terminal-host {\n    min-width: 0;\n    overflow: hidden;\n    max-width: 100%;");
    expect(css).toContain(".terminal-host .xterm,\n.terminal-host .xterm-screen,\n.terminal-host .xterm-viewport {\n  width: 100%;\n  height: 100%;");
    const terminalCanvasBlock = css.match(/\.terminal-host \.xterm-screen canvas \{[^}]+\}/)?.[0] ?? "";
    expect(terminalCanvasBlock).toContain("display: block;");
    expect(terminalCanvasBlock).toContain("background: var(--color-terminal-bg);");
    expect(css).not.toContain(".terminal-host,\n  .terminal-host canvas");
    expect(css).toContain('.terminal-host textarea[aria-label="Terminal input"]');
    expect(css).toContain(".daemon-cpu-bar-chart {\n    display: none;");
    expect(css).toContain("minmax(124px, 1.25fr);");
    expect(css).toContain(".daemon-status-strip .daemon-status-network strong {\n    min-width: max-content;");
    const helperTextareaBlock =
      css.match(/\.terminal-host textarea\[aria-label="Terminal input"\] \{[^}]+\}/)?.[0] ?? "";
    expect(helperTextareaBlock).toContain("helper textarea 需要保留 focus/paste/IME 能力");
    expect(helperTextareaBlock).toContain("min-height: 0 !important;");
    expect(helperTextareaBlock).toContain("caret-color: transparent !important;");
    expect(helperTextareaBlock).not.toContain("position: fixed !important;");
    expect(helperTextareaBlock).not.toContain("width: 1px !important;");
    const hostFocusBlock = css.match(/\.terminal-host:focus,\n\.terminal-host:focus-within \{[^}]+\}/)?.[0] ?? "";
    expect(hostFocusBlock).toContain("outline: none;");
    expect(hostFocusBlock).toContain("caret-color: transparent !important;");
  });

  it("keeps terminal search result text in its own grid column before the close button", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("grid-template-columns: minmax(0, 1fr) repeat(3, 28px) minmax(54px, max-content) 28px;");
    expect(css).toContain(".terminal-search-count {\n  min-width: 54px;");
  });

  it("keeps toolbar icons and labels vertically centered in buttons", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const buttonBlock = css.match(/button \{[^}]+\}/)?.[0] ?? "";

    expect(buttonBlock).toContain("display: inline-flex;");
    expect(buttonBlock).toContain("align-items: center;");
    expect(buttonBlock).not.toContain("align-items: flex-end;");
  });
});
