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
    expect(css).toContain("--terminal-mobile-shortcuts-height: 42px;");
    expect(css).toContain(".app-shell.mobile-keyboard-open .terminal-scrollport {\n    /* 中文注释：快捷键栏是 overlay，键盘打开时终端可视底边要停在快捷键栏上方。 */\n    margin-bottom: var(--terminal-mobile-shortcuts-height);");
    expect(css).not.toContain(".app-shell.mobile-keyboard-open .terminal-pane {\n    grid-template-rows: minmax(0, 1fr) 42px;");
    const mobileShortcutsBlock = css.match(/\.terminal-mobile-shortcuts \{[^}]+\}/)?.[0] ?? "";
    expect(mobileShortcutsBlock).toContain("position: absolute;");
    expect(mobileShortcutsBlock).toContain("bottom: 0;");
    expect(css).toContain(".terminal-host {\n    min-width: 0;\n    overflow: hidden;\n    max-width: 100%;");
    expect(css).toContain(".terminal-host .xterm,\n.terminal-host .xterm-screen,\n.terminal-host .xterm-viewport {\n  width: 100%;\n  height: 100%;");
    const terminalSurfaceBlock =
      css.match(/\.terminal-host \.xterm,\n\.terminal-host \.xterm-screen,\n\.terminal-host \.xterm-viewport \{[^}]+\}/)?.[0] ?? "";
    expect(terminalSurfaceBlock).toContain("background: var(--color-terminal-bg);");
    const terminalCanvasBlock = css.match(/\.terminal-host \.xterm-screen canvas \{[^}]+\}/)?.[0] ?? "";
    expect(terminalCanvasBlock).toContain("display: block;");
    expect(terminalCanvasBlock).toContain("background: var(--color-terminal-bg);");
    const redrawMaskBlock =
      css.match(/\.terminal-host\[data-termd-snapshot-redraw="true"\] \.xterm-screen,\n\.terminal-host\[data-termd-resize-stabilizing="true"\] \.xterm-screen \{[^}]+\}/)?.[0] ?? "";
    expect(redrawMaskBlock).toContain("opacity: 0;");
    expect(redrawMaskBlock).not.toContain("visibility: hidden;");
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

  it("keeps terminal open progress labels and timings in stable responsive columns", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("left: calc(var(--terminal-frame-padding) + 8px);");
    expect(css).toContain("width: min(340px, 100%);");
    expect(css).toContain(".terminal-open-progress-button,\n.terminal-open-progress-popover {\n  pointer-events: auto;");
    expect(css).toContain("grid-template-columns: 16px minmax(0, 1fr) 64px;");
    expect(css).toContain("font-variant-numeric: tabular-nums;");
    expect(css).not.toContain(".terminal-search-control");
  });

  it("keeps toolbar icons and labels vertically centered in buttons", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const buttonBlock = css.match(/button \{[^}]+\}/)?.[0] ?? "";

    expect(buttonBlock).toContain("display: inline-flex;");
    expect(buttonBlock).toContain("align-items: center;");
    expect(buttonBlock).not.toContain("align-items: flex-end;");
  });
});
