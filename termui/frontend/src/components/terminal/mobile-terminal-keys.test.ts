import { describe, expect, it } from "vitest";
import {
  clearModifiers,
  consumeModifiersOnce,
  encodeTerminalInputData,
  encodeTerminalKey,
  toggleModifier,
  type ModifierState,
  type TerminalKey,
} from "./mobile-terminal-keys";

const NO_MODIFIERS: ModifierState = { ctrl: false, alt: false, shift: false };

describe("mobile terminal modifier state", () => {
  it("toggles modifiers immutably, clears them, and consumes them once", () => {
    const initial: ModifierState = { ctrl: false, alt: true, shift: false };

    const toggled = toggleModifier(initial, "ctrl");
    expect(toggled).toEqual({ ctrl: true, alt: true, shift: false });
    expect(initial).toEqual({ ctrl: false, alt: true, shift: false });

    expect(clearModifiers(toggled)).toEqual({ ctrl: false, alt: false, shift: false });
    expect(consumeModifiersOnce(toggled)).toEqual({
      modifiers: { ctrl: true, alt: true, shift: false },
      nextModifiers: { ctrl: false, alt: false, shift: false },
    });
  });
});

describe("mobile terminal key encoding", () => {
  it.each<[TerminalKey, string]>([
    ["Esc", "\x1b"],
    ["Tab", "\t"],
    ["ShiftTab", "\x1b[Z"],
    ["Home", "\x1b[H"],
    ["End", "\x1b[F"],
    ["PgUp", "\x1b[5~"],
    ["PgDn", "\x1b[6~"],
    ["Insert", "\x1b[2~"],
    ["Delete", "\x1b[3~"],
    ["F1", "\x1bOP"],
    ["F2", "\x1bOQ"],
    ["F3", "\x1bOR"],
    ["F4", "\x1bOS"],
    ["F5", "\x1b[15~"],
    ["F6", "\x1b[17~"],
    ["F7", "\x1b[18~"],
    ["F8", "\x1b[19~"],
    ["F9", "\x1b[20~"],
    ["F10", "\x1b[21~"],
    ["F11", "\x1b[23~"],
    ["F12", "\x1b[24~"],
  ])("encodes %s and clears sticky modifiers", (key, data) => {
    expect(encodeTerminalKey(key, NO_MODIFIERS)).toEqual({ data, nextModifiers: NO_MODIFIERS });
  });

  it.each<[TerminalKey, string]>([
    ["ArrowUp", "A"],
    ["ArrowDown", "B"],
    ["ArrowRight", "C"],
    ["ArrowLeft", "D"],
  ])("encodes bare %s according to DECCKM", (key, suffix) => {
    expect(encodeTerminalKey(key, NO_MODIFIERS).data).toBe(`\x1b[${suffix}`);
    expect(encodeTerminalKey(key, NO_MODIFIERS, { applicationCursorKeys: true }).data)
      .toBe(`\x1bO${suffix}`);
  });

  it.each([
    [{ ctrl: false, alt: false, shift: true }, 2],
    [{ ctrl: false, alt: true, shift: false }, 3],
    [{ ctrl: true, alt: false, shift: false }, 5],
    [{ ctrl: true, alt: true, shift: true }, 8],
  ] as const)("encodes modified arrows with xterm modifier %s", (modifiers, modifierParameter) => {
    expect(encodeTerminalKey("ArrowUp", modifiers, { applicationCursorKeys: true })).toEqual({
      data: `\x1b[1;${modifierParameter}A`,
      nextModifiers: NO_MODIFIERS,
    });
  });

  it.each([
    [{ kind: "character", value: "x" }, NO_MODIFIERS, "x"],
    [{ kind: "character", value: "x" }, { ctrl: false, alt: false, shift: true }, "X"],
    [{ kind: "character", value: "a" }, { ctrl: true, alt: false, shift: false }, "\x01"],
    [{ kind: "character", value: "Z" }, { ctrl: true, alt: false, shift: false }, "\x1a"],
    [{ kind: "character", value: "x" }, { ctrl: false, alt: true, shift: false }, "\x1bx"],
    [{ kind: "character", value: "a" }, { ctrl: true, alt: true, shift: true }, "\x1b\x01"],
  ] as const)("encodes character %s with sticky modifiers", (key, modifiers, data) => {
    expect(encodeTerminalKey(key, modifiers)).toEqual({ data, nextModifiers: NO_MODIFIERS });
  });

  it.each([
    ["@", "\x00"],
    ["[", "\x1b"],
    ["\\", "\x1c"],
    ["]", "\x1d"],
    ["^", "\x1e"],
    ["_", "\x1f"],
  ] as const)("encodes Ctrl+%s as an ASCII control character", (value, data) => {
    expect(encodeTerminalKey(
      { kind: "character", value },
      { ctrl: true, alt: false, shift: false },
    )).toEqual({ data, nextModifiers: NO_MODIFIERS });
  });

  it("rejects values that are not exactly one character", () => {
    expect(() => encodeTerminalKey({ kind: "character", value: "ab" }, NO_MODIFIERS))
      .toThrow("exactly one character");
  });

  it("uses SS3 for bare Home and End in DECCKM mode", () => {
    expect(encodeTerminalKey("Home", NO_MODIFIERS, { applicationCursorKeys: true }).data).toBe("\x1bOH");
    expect(encodeTerminalKey("End", NO_MODIFIERS, { applicationCursorKeys: true }).data).toBe("\x1bOF");
  });

  it("treats Shift+Tab as backtab", () => {
    expect(encodeTerminalKey("Tab", { ctrl: false, alt: false, shift: true })).toEqual({
      data: "\x1b[Z",
      nextModifiers: NO_MODIFIERS,
    });
  });

  it.each<[TerminalKey, ModifierState, string]>([
    ["Home", { ctrl: true, alt: true, shift: true }, "\x1b[1;8H"],
    ["End", { ctrl: true, alt: false, shift: false }, "\x1b[1;5F"],
    ["Insert", { ctrl: true, alt: false, shift: false }, "\x1b[2;5~"],
    ["Delete", { ctrl: false, alt: true, shift: false }, "\x1b[3;3~"],
    ["PgUp", { ctrl: false, alt: false, shift: true }, "\x1b[5;2~"],
    ["PgDn", { ctrl: false, alt: true, shift: false }, "\x1b[6;3~"],
    ["F1", { ctrl: true, alt: false, shift: false }, "\x1b[1;5P"],
    ["F4", { ctrl: false, alt: true, shift: true }, "\x1b[1;4S"],
    ["F5", { ctrl: true, alt: true, shift: true }, "\x1b[15;8~"],
    ["F12", { ctrl: false, alt: true, shift: true }, "\x1b[24;4~"],
  ])("encodes modified %s using the xterm modifier parameter", (key, modifiers, data) => {
    expect(encodeTerminalKey(key, modifiers, { applicationCursorKeys: true })).toEqual({
      data,
      nextModifiers: NO_MODIFIERS,
    });
  });
});

describe("mobile terminal input data encoding", () => {
  it.each([
    ["Ctrl+c", "c", { ctrl: true, alt: false, shift: false }, "\x03"],
    ["Alt+x", "x", { ctrl: false, alt: true, shift: false }, "\x1bx"],
    ["Shift+a", "a", { ctrl: false, alt: false, shift: true }, "A"],
    ["Shift+Tab", "\t", { ctrl: false, alt: false, shift: true }, "\x1b[Z"],
    ["unsupported Ctrl symbol", "|", { ctrl: true, alt: false, shift: false }, "|"],
    ["Unicode IME input", "你", { ctrl: true, alt: false, shift: true }, "你"],
  ] as const)("applies %s once and clears all modifiers", (_label, data, modifiers, expected) => {
    expect(encodeTerminalInputData(data, modifiers)).toEqual({
      data: expected,
      nextModifiers: NO_MODIFIERS,
    });
  });

  it("adds Escape to unknown multi-character data when Alt is active", () => {
    expect(encodeTerminalInputData(
      "unknown",
      { ctrl: false, alt: true, shift: false },
    )).toEqual({ data: "\x1bunknown", nextModifiers: NO_MODIFIERS });
  });

  it.each<[string, TerminalKey]>([
    ["\x1b[A", "ArrowUp"],
    ["\x1b[B", "ArrowDown"],
    ["\x1b[C", "ArrowRight"],
    ["\x1b[D", "ArrowLeft"],
    ["\x1bOA", "ArrowUp"],
    ["\x1bOB", "ArrowDown"],
    ["\x1bOC", "ArrowRight"],
    ["\x1bOD", "ArrowLeft"],
    ["\x1b[H", "Home"],
    ["\x1b[F", "End"],
    ["\x1bOH", "Home"],
    ["\x1bOF", "End"],
    ["\x1b[5~", "PgUp"],
    ["\x1b[6~", "PgDn"],
    ["\x1b[2~", "Insert"],
    ["\x1b[3~", "Delete"],
    ["\x1bOP", "F1"],
    ["\x1bOQ", "F2"],
    ["\x1bOR", "F3"],
    ["\x1bOS", "F4"],
    ["\x1b[15~", "F5"],
    ["\x1b[17~", "F6"],
    ["\x1b[18~", "F7"],
    ["\x1b[19~", "F8"],
    ["\x1b[20~", "F9"],
    ["\x1b[21~", "F10"],
    ["\x1b[23~", "F11"],
    ["\x1b[24~", "F12"],
  ])("recognizes %j and applies Alt through the named-key encoder", (data, key) => {
    const modifiers: ModifierState = { ctrl: false, alt: true, shift: false };
    expect(encodeTerminalInputData(data, modifiers)).toEqual(
      encodeTerminalKey(key, modifiers),
    );
  });
});
