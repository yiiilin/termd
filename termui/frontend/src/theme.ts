import type { BrowserThemePreference, EffectiveTheme } from "./protocol/types";

export interface TerminalThemeColors {
  background: string;
  foreground: string;
  cursor: string;
  selectionBackground: string;
}

export function resolveTheme(preference: BrowserThemePreference, systemTheme: EffectiveTheme): EffectiveTheme {
  if (preference === "dark" || preference === "light") {
    return preference;
  }
  return systemTheme;
}

export function terminalTheme(theme: EffectiveTheme): TerminalThemeColors {
  if (theme === "light") {
    return {
      background: "#eae4ca",
      foreground: "#5c6a72",
      cursor: "#8da101",
      selectionBackground: "#d3c6aa",
    };
  }
  return {
    background: "#293136",
    foreground: "#d3c6aa",
    cursor: "#a7c080",
    selectionBackground: "#5d6b66",
  };
}

export function monacoTheme(theme: EffectiveTheme): "vs" | "vs-dark" {
  return theme === "light" ? "vs" : "vs-dark";
}
