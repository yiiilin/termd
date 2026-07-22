import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("mobile layout regressions", () => {
  it("keeps browser zoom available while opting into safe-area layout", () => {
    const html = readFileSync(resolve(process.cwd(), "index.html"), "utf8");
    const viewport = html.match(/<meta\s+name="viewport"\s+content="([^"]+)"/m)?.[1];

    expect(viewport).toContain("width=device-width");
    expect(viewport).toContain("viewport-fit=cover");
    expect(viewport).not.toContain("maximum-scale");
    expect(viewport).not.toContain("user-scalable=no");
  });

  it("clamps the terminal pane and xterm host so mobile width cannot overflow the viewport", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain(".toolbar-actions {\n    display: none;");
    expect(css).toContain(".toolbar-title {\n    flex: 1 1 auto;\n    max-width: none;\n    overflow: hidden;");
    expect(css).toContain(".toolbar-title .toolbar-session-name {\n  flex: 1 1 auto;");
    expect(css).toContain(".toolbar-title .toolbar-connection-anomaly {");
    expect(css).toContain("@media (max-width: 360px) {\n  .toolbar-session-size {\n    display: none;");
    expect(css).toContain(".terminal-scrollport {\n    overflow: hidden;");
    expect(css).toContain("top: var(--termd-visual-viewport-offset-top, 0px);");
    expect(css).toContain(".app-shell.mobile-terminal-input.mobile-keyboard-open {");
    expect(css).toContain(".app-shell.mobile-keyboard-open .workspace {\n    grid-template-rows: var(--mobile-toolbar-height) minmax(0, 1fr);");
    expect(css).toContain(".app-shell.mobile-keyboard-open .daemon-status-strip {\n    display: none;");
    expect(css).toContain("--mobile-toolbar-height: calc(44px + env(safe-area-inset-top, 0px));");
    expect(css).toContain("--mobile-status-height: calc(30px + env(safe-area-inset-bottom, 0px));");
    expect(css).toContain("--terminal-mobile-shortcuts-content-height: 44px;");
    expect(css).toContain("--terminal-mobile-shortcuts-safe-area: env(safe-area-inset-bottom, 0px);");
    expect(css).toContain(
      ".terminal-pane {\n  --terminal-mobile-shortcuts-height: calc(\n    " +
      "var(--terminal-mobile-shortcuts-content-height) + var(--terminal-mobile-shortcuts-safe-area)",
    );
    expect(css).toContain(".terminal-pane.mobile-quick-keys-expanded {");
    expect(css).toContain("--terminal-mobile-shortcuts-content-height: 132px;");
    expect(css).toContain(".app-shell.mobile-terminal-input.mobile-keyboard-open {\n  --terminal-mobile-shortcuts-safe-area: 0px;");
    expect(css).toContain(".app-shell.mobile-keyboard-open .terminal-scrollport {\n  /* 中文注释：快捷键栏是 overlay，键盘打开时终端可视底边要停在快捷键栏上方。 */\n  margin-bottom: var(--terminal-mobile-shortcuts-height);");
    expect(css).not.toContain(".app-shell.mobile-keyboard-open .terminal-pane {\n    grid-template-rows: minmax(0, 1fr) 42px;");
    const mobileShortcutsBlock = css.match(/\.terminal-mobile-shortcuts \{[^}]+\}/)?.[0] ?? "";
    expect(mobileShortcutsBlock).toContain("position: absolute;");
    expect(mobileShortcutsBlock).toContain("bottom: 0;");
    expect(mobileShortcutsBlock).toContain("height: var(--terminal-mobile-shortcuts-height);");
    expect(mobileShortcutsBlock).toContain("padding-bottom: var(--terminal-mobile-shortcuts-safe-area);");
    expect(css).toContain(".terminal-quick-keys-main {");
    expect(css).toContain("-webkit-overflow-scrolling: touch;");
    expect(css).toContain("touch-action: pan-x;");
    expect(css).toContain(".terminal-mobile-shortcut-button {\n  flex: 0 0 44px;");
    expect(css).toContain(".terminal-quick-keys-tab {\n  min-width: 0;\n  min-height: 44px;");
    expect(css).toContain("@media (hover: none), (pointer: coarse) {");
    expect(css).toContain("button:not(.mobile-backdrop),\n  input:not(.sr-only),");
    expect(css).toContain("min-block-size: 44px;");
    expect(css).toContain("inset: var(--mobile-toolbar-height) 0 var(--mobile-status-height) 0;");
    expect(css).toContain("bottom: calc(12px + env(safe-area-inset-bottom, 0px));");
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
    expect(css).toContain("grid-template-columns: minmax(0, 1fr) auto;");
    expect(css).toContain(".daemon-status-strip .daemon-status-cpu,\n  .daemon-status-strip .daemon-status-memory,\n  .daemon-status-strip .daemon-status-disk {\n    display: none;");
    expect(css).toContain("width: clamp(128px, 44vw, 176px);");
    expect(css).toContain(".daemon-status-strip .daemon-status-network strong {\n    min-width: 0;\n    font-variant-numeric: tabular-nums;");
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

  it("keeps narrow settings labels readable and short touch workspaces within their rows", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const narrowRules = cssPreferenceBlock(css, "max-width: 360px");
    const coarseRules = cssPreferenceBlock(css, "hover: none), (pointer: coarse");
    const shortWorkspaceRules = cssPreferenceBlock(css, "min-width: 761px) and (max-height: 490px");

    expect(cssDeclarations(narrowRules, ".settings-segmented span")).toMatchObject({
      "min-height": "44px",
      "padding-inline": "4px",
      "line-height": "1.2",
      "white-space": "normal",
    });
    expect(cssDeclarations(coarseRules, ":root")).toMatchObject({
      "--deck-toolbar-height": "44px",
    });
    expect(cssDeclarations(coarseRules, ".settings-segmented label,\n  .settings-segmented span")).toMatchObject({
      "min-height": "44px",
    });
    expect(cssDeclarations(shortWorkspaceRules, ".workspace")).toMatchObject({
      "grid-template-rows": "var(--deck-toolbar-height) minmax(0, 1fr) var(--deck-status-height)",
    });
  });

  it("honors contrast, transparency, motion, and immediate press preferences", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("button:not(.mobile-backdrop):not(.toolbar-title-button):not(.session-open-button):active:not(:disabled) {");
    expect(css).toContain("transform: scale(0.97);");
    expect(css).toContain("transition: transform 100ms ease-out;");
    expect(css).toContain(".toolbar-title-button:active:not(:disabled) {");
    expect(css).toContain(".session-open-button:active:not(:disabled) {");
    expect(css).toContain("touch-action: none;");
    const contrastBlock = cssPreferenceBlock(css, "prefers-contrast: more");
    expect(contrastBlock).toContain(".destructive-action-dialog,");
    expect(contrastBlock).toContain(".unsaved-file-dialog,");
    expect(contrastBlock).toContain(".clients-popover .panel,");
    expect(contrastBlock).not.toContain(".destructive-dialog,");
    const transparencyBlock = cssPreferenceBlock(css, "prefers-reduced-transparency: reduce");
    expect(transparencyBlock).toContain("--color-bg-shell: #1b2024;");
    expect(transparencyBlock).toContain("--color-floating-gradient: linear-gradient(90deg, #1b2024, #343d43 16px);");
    expect(transparencyBlock).toContain(".toolbar,\n  .mobile-menu-popover,\n  .terminal-direction-pad {\n    backdrop-filter: none;");
    expect(transparencyBlock).toContain(".terminal-direction-pad {\n    backdrop-filter: none;");
    const motionBlock = cssPreferenceBlock(css, "prefers-reduced-motion: reduce");
    expect(motionBlock).toContain("transition-duration: 0.01ms !important;");
    expect(motionBlock).toContain("transform: translate(-50%, 0);");
    expect(motionBlock).toContain(".toolbar-title-refreshing .toolbar-title-pull-indicator svg,");
  });
});

function cssPreferenceBlock(css: string, query: string): string {
  const marker = `@media (${query})`;
  const blocks: string[] = [];
  let searchFrom = 0;
  while (searchFrom < css.length) {
    const start = css.indexOf(marker, searchFrom);
    if (start < 0) break;
    const openBrace = css.indexOf("{", start + marker.length);
    expect(openBrace, query).toBeGreaterThan(start);
    let depth = 1;
    for (let index = openBrace + 1; index < css.length; index += 1) {
      if (css[index] === "{") depth += 1;
      if (css[index] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push(css.slice(openBrace + 1, index));
        searchFrom = index + 1;
        break;
      }
    }
    if (depth !== 0) throw new Error(`unterminated CSS preference block: ${query}`);
  }
  expect(blocks.length, query).toBeGreaterThan(0);
  return blocks.join("\n");
}

function cssDeclarations(css: string, selector: string): Record<string, string> {
  const marker = `${selector} {`;
  const start = css.indexOf(marker);
  expect(start, selector).toBeGreaterThanOrEqual(0);
  const openBrace = css.indexOf("{", start + selector.length);
  const closeBrace = css.indexOf("}", openBrace + 1);
  expect(closeBrace, selector).toBeGreaterThan(openBrace);

  return Object.fromEntries(
    [...css.slice(openBrace + 1, closeBrace).matchAll(/([\w-]+)\s*:\s*([^;]+);/g)].map((match) => [
      match[1],
      match[2].trim(),
    ]),
  );
}
