import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { terminalTheme } from "./theme";

describe("terminal theme contrast", () => {
  it.each(["light", "dark"] as const)("keeps %s foreground readable against the terminal background", (theme) => {
    const colors = terminalTheme(theme);

    expect(contrastRatio(colors.foreground, colors.background)).toBeGreaterThanOrEqual(4.5);
    expect(contrastRatio(colors.cursor, colors.background)).toBeGreaterThanOrEqual(3);
    expect(contrastRatio(colors.selectionForeground, colors.selectionBackground)).toBeGreaterThanOrEqual(4.5);
  });

  it.each([
    ["dark", ":root"],
    ["light", ':root[data-theme="light"]'],
  ] as const)("keeps %s semantic text colors readable on their real surfaces", (_theme, selector) => {
    const colors = cssCustomProperties(selector);

    for (const [foreground, background] of [
      ["--color-text", "--color-bg-page"],
      ["--color-text", "--color-surface"],
      ["--color-text", "--color-surface-alt"],
      ["--color-text-muted", "--color-surface-alt"],
      ["--color-text-subtle", "--color-surface-alt"],
      ["--color-text-dim", "--color-surface-deep"],
      ["--color-text-status", "--color-surface-deep"],
      ["--color-danger-text", "--color-danger-bg"],
      ["--color-danger-text", "--color-danger-bg-strong"],
      ["--color-accent-text", "--color-surface-selected"],
      ["--color-accent-text", "--color-surface-active"],
      ["--color-info-text", "--color-surface-alt"],
      ["--color-info-text", "--color-surface-hover"],
      ["--color-info-text", "--color-surface-active"],
    ] as const) {
      expect(
        contrastRatio(requiredColor(colors, foreground), requiredColor(colors, background)),
        `${foreground} on ${background}`,
      ).toBeGreaterThanOrEqual(4.5);
    }

    for (const background of ["--color-surface-raised", "--color-surface-sunken"] as const) {
      expect(
        contrastRatio(requiredColor(colors, "--color-focus"), requiredColor(colors, background)),
        `--color-focus on ${background}`,
      ).toBeGreaterThanOrEqual(3);
    }
  });

  it.each([
    ["dark", ":root"],
    ["light", ':root[data-theme="light"]'],
  ] as const)("keeps %s status labels readable on tinted and selected surfaces", (_theme, selector) => {
    const colors = cssCustomProperties(selector);

    for (const status of ["success", "info", "warning", "danger"] as const) {
      const foreground = requiredColor(colors, `--color-${status}-text`);
      const tint = requiredColor(colors, status === "danger" ? "--color-danger" : `--color-${status}`);
      for (const [backgroundToken, tintPercent] of [
        ["--color-surface-raised", 13],
        ["--color-surface-selected", 13],
        ["--color-surface", 10],
        ["--color-surface", 12],
      ] as const) {
        const background = mixSrgb(tint, requiredColor(colors, backgroundToken), tintPercent / 100);
        expect(
          contrastRatio(foreground, background),
          `${status} text on ${tintPercent}% ${backgroundToken}`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  it("routes compact selected and status labels through contrast-safe text tokens", () => {
    expect(cssRule('.files-tab[aria-selected="true"]')).toContain("color: var(--color-accent-text);");
    expect(cssRule(".git-worktree-current")).toContain("color: var(--color-info-text);");
    expect(cssRule(".git-change-status")).toContain("color: var(--color-info-text);");
  });

  it.each(["light", "dark"] as const)("keeps %s CSS terminal colors aligned with xterm", (theme) => {
    const selector = theme === "light" ? ':root[data-theme="light"]' : ":root";
    const colors = cssCustomProperties(selector);
    const terminal = terminalTheme(theme);

    expect(requiredColor(colors, "--color-terminal-bg")).toBe(terminal.background);
    expect(requiredColor(colors, "--color-terminal-fg")).toBe(terminal.foreground);
  });
});

function requiredColor(colors: Map<string, string>, token: string): string {
  const value = colors.get(token);
  expect(value, token).toBeDefined();
  return value!;
}

function mixSrgb(foreground: string, background: string, foregroundWeight: number): string {
  const foregroundChannels = hexChannels(foreground);
  const backgroundChannels = hexChannels(background);
  return `#${foregroundChannels.map((channel, index) =>
    Math.round(channel * foregroundWeight + backgroundChannels[index] * (1 - foregroundWeight))
      .toString(16)
      .padStart(2, "0")
  ).join("")}`;
}

function hexChannels(hex: string): number[] {
  return hex.slice(1).match(/.{2}/g)?.map((channel) => Number.parseInt(channel, 16)) ?? [];
}

function cssCustomProperties(selector: string): Map<string, string> {
  const block = cssRule(selector);
  return new Map(
    Array.from(block.matchAll(/(--[\w-]+):\s*(#[0-9a-f]{6})\s*;/giu), (match) => [match[1], match[2]]),
  );
}

function cssRule(selector: string): string {
  const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
  const baseCss = cssWithoutMediaBlocks(css);
  const blocks = Array.from(baseCss.matchAll(/([^{}]+)\{([^{}]*)\}/g))
    .filter((match) => {
      const ruleSelector = match[1].replace(/\/\*[\s\S]*?\*\//g, "").trim();
      return !ruleSelector.includes(",") && ruleSelector === selector;
    })
    .map((match) => match[2]);
  return blocks.at(-1) ?? "";
}

function cssWithoutMediaBlocks(css: string): string {
  let result = "";
  let cursor = 0;
  while (cursor < css.length) {
    const start = css.indexOf("@media", cursor);
    if (start < 0) return result + css.slice(cursor);
    result += css.slice(cursor, start);
    const openBrace = css.indexOf("{", start + "@media".length);
    if (openBrace < 0) throw new Error("unterminated CSS media query");
    let depth = 1;
    for (let index = openBrace + 1; index < css.length; index += 1) {
      if (css[index] === "{") depth += 1;
      if (css[index] === "}") depth -= 1;
      if (depth === 0) {
        cursor = index + 1;
        break;
      }
    }
    if (depth !== 0) throw new Error("unterminated CSS media query");
  }
  return result;
}

function contrastRatio(first: string, second: string): number {
  const firstLuminance = relativeLuminance(first);
  const secondLuminance = relativeLuminance(second);
  const lighter = Math.max(firstLuminance, secondLuminance);
  const darker = Math.min(firstLuminance, secondLuminance);
  return (lighter + 0.05) / (darker + 0.05);
}

function relativeLuminance(hex: string): number {
  const channels = hex.slice(1).match(/.{2}/g)?.map((channel) => Number.parseInt(channel, 16) / 255) ?? [];
  const [red = 0, green = 0, blue = 0] = channels.map((channel) =>
    channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4,
  );
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
}
