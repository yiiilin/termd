import type { BrowserThemePreference, EffectiveTheme } from "./protocol/types";

export interface TerminalThemeColors {
  background: string;
  foreground: string;
  cursor: string;
  selectionBackground: string;
  selectionForeground: string;
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
      background: "#0e1214",
      foreground: "#dce4df",
      cursor: "#72c28c",
      selectionBackground: "#30463b",
      selectionForeground: "#f2f5f3",
    };
  }
  return {
    background: "#0b0e10",
    foreground: "#d8dedb",
    cursor: "#6fbd87",
    selectionBackground: "#30463b",
    selectionForeground: "#f1f4f2",
  };
}

export function monacoTheme(theme: EffectiveTheme): "vs" | "vs-dark" {
  return theme === "light" ? "vs" : "vs-dark";
}
