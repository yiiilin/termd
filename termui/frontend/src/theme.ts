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
      background: "#eff1ec",
      foreground: "#384044",
      cursor: "#536b32",
      selectionBackground: "#cbd6c5",
      selectionForeground: "#20282c",
    };
  }
  return {
    background: "#181c1f",
    foreground: "#d8cfb9",
    cursor: "#a7c080",
    selectionBackground: "#48564f",
    selectionForeground: "#f6f7f2",
  };
}

export function monacoTheme(theme: EffectiveTheme): "vs" | "vs-dark" {
  return theme === "light" ? "vs" : "vs-dark";
}
