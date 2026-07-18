export interface ModifierState {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export type ModifierKey = keyof ModifierState;

export interface ConsumedModifierState {
  modifiers: ModifierState;
  nextModifiers: ModifierState;
}

export type TerminalNamedKey =
  | "Esc"
  | "Tab"
  | "ShiftTab"
  | "Home"
  | "End"
  | "PgUp"
  | "PgDn"
  | "Insert"
  | "Delete"
  | "F1"
  | "F2"
  | "F3"
  | "F4"
  | "F5"
  | "F6"
  | "F7"
  | "F8"
  | "F9"
  | "F10"
  | "F11"
  | "F12";

export type TerminalArrowKey = "ArrowUp" | "ArrowDown" | "ArrowRight" | "ArrowLeft";

export interface TerminalCharacterKey {
  kind: "character";
  value: string;
}

export type TerminalKey = TerminalNamedKey | TerminalArrowKey | TerminalCharacterKey;

export interface EncodedTerminalKey {
  data: string;
  nextModifiers: ModifierState;
}

export interface EncodeTerminalKeyOptions {
  applicationCursorKeys?: boolean;
}

const NAMED_KEY_DATA: Record<TerminalNamedKey, string> = {
  Esc: "\x1b",
  Tab: "\t",
  ShiftTab: "\x1b[Z",
  Home: "\x1b[H",
  End: "\x1b[F",
  PgUp: "\x1b[5~",
  PgDn: "\x1b[6~",
  Insert: "\x1b[2~",
  Delete: "\x1b[3~",
  F1: "\x1bOP",
  F2: "\x1bOQ",
  F3: "\x1bOR",
  F4: "\x1bOS",
  F5: "\x1b[15~",
  F6: "\x1b[17~",
  F7: "\x1b[18~",
  F8: "\x1b[19~",
  F9: "\x1b[20~",
  F10: "\x1b[21~",
  F11: "\x1b[23~",
  F12: "\x1b[24~",
};

const ARROW_SUFFIX: Record<TerminalArrowKey, string> = {
  ArrowUp: "A",
  ArrowDown: "B",
  ArrowRight: "C",
  ArrowLeft: "D",
};

const HOME_END_SUFFIX: Partial<Record<TerminalNamedKey, string>> = {
  Home: "H",
  End: "F",
};

const SS3_FUNCTION_SUFFIX: Partial<Record<TerminalNamedKey, string>> = {
  F1: "P",
  F2: "Q",
  F3: "R",
  F4: "S",
};

const CSI_TILDE_CODE: Partial<Record<TerminalNamedKey, string>> = {
  Insert: "2",
  Delete: "3",
  PgUp: "5",
  PgDn: "6",
  F5: "15",
  F6: "17",
  F7: "18",
  F8: "19",
  F9: "20",
  F10: "21",
  F11: "23",
  F12: "24",
};

const TERMINAL_INPUT_KEYS = new Map<string, TerminalKey>([
  ...Object.entries(NAMED_KEY_DATA).map(([key, data]) => (
    [data, key as TerminalNamedKey] as [string, TerminalKey]
  )),
  ...Object.entries(ARROW_SUFFIX).flatMap(([key, suffix]) => ([
    [`\x1b[${suffix}`, key as TerminalArrowKey],
    [`\x1bO${suffix}`, key as TerminalArrowKey],
  ] as Array<[string, TerminalKey]>)),
  ["\x1bOH", "Home"],
  ["\x1bOF", "End"],
]);

function modifierParameter(modifiers: ModifierState): number {
  return 1 + Number(modifiers.shift) + 2 * Number(modifiers.alt) + 4 * Number(modifiers.ctrl);
}

export function toggleModifier(state: ModifierState, modifier: ModifierKey): ModifierState {
  return { ...state, [modifier]: !state[modifier] };
}

export function clearModifiers(_state: ModifierState): ModifierState {
  return { ctrl: false, alt: false, shift: false };
}

export function consumeModifiersOnce(state: ModifierState): ConsumedModifierState {
  return {
    modifiers: { ...state },
    nextModifiers: clearModifiers(state),
  };
}

export function encodeTerminalKey(
  key: TerminalKey,
  modifiers: ModifierState,
  options: EncodeTerminalKeyOptions = {},
): EncodedTerminalKey {
  if (typeof key !== "string") {
    const characters = Array.from(key.value);
    if (characters.length !== 1) {
      throw new Error("terminal character key must contain exactly one character");
    }
    let data = characters[0];
    if (modifiers.shift && /^[a-z]$/.test(data)) {
      data = data.toUpperCase();
    }
    if (modifiers.ctrl) {
      const upper = data.toUpperCase();
      if (!/^[\x40-\x5f]$/.test(upper)) {
        throw new Error("Ctrl modifier is only supported for ASCII @ through _ characters");
      }
      data = String.fromCharCode(upper.charCodeAt(0) & 0x1f);
    }
    if (modifiers.alt) {
      data = `\x1b${data}`;
    }
    return { data, nextModifiers: clearModifiers(modifiers) };
  }
  const arrowSuffix = ARROW_SUFFIX[key as TerminalArrowKey];
  const hasModifiers = modifiers.ctrl || modifiers.alt || modifiers.shift;
  let data: string;
  if (arrowSuffix) {
    if (hasModifiers) {
      data = `\x1b[1;${modifierParameter(modifiers)}${arrowSuffix}`;
    } else {
      data = `${options.applicationCursorKeys ? "\x1bO" : "\x1b["}${arrowSuffix}`;
    }
  } else {
    const namedKey = key as TerminalNamedKey;
    const homeEndSuffix = HOME_END_SUFFIX[namedKey];
    const functionSuffix = SS3_FUNCTION_SUFFIX[namedKey];
    const tildeCode = CSI_TILDE_CODE[namedKey];
    if (namedKey === "Tab" && modifiers.shift) {
      data = "\x1b[Z";
      if (modifiers.alt) data = `\x1b${data}`;
    } else if (homeEndSuffix && hasModifiers) {
      data = `\x1b[1;${modifierParameter(modifiers)}${homeEndSuffix}`;
    } else if (homeEndSuffix && options.applicationCursorKeys) {
      data = `\x1bO${homeEndSuffix}`;
    } else if (functionSuffix && hasModifiers) {
      data = `\x1b[1;${modifierParameter(modifiers)}${functionSuffix}`;
    } else if (tildeCode && hasModifiers) {
      data = `\x1b[${tildeCode};${modifierParameter(modifiers)}~`;
    } else {
      data = NAMED_KEY_DATA[namedKey];
      if (modifiers.alt) data = `\x1b${data}`;
    }
  }
  return {
    data,
    nextModifiers: clearModifiers(modifiers),
  };
}

export function encodeTerminalInputData(
  data: string,
  modifiers: ModifierState,
  options: EncodeTerminalKeyOptions = {},
): EncodedTerminalKey {
  const key = TERMINAL_INPUT_KEYS.get(data);
  if (key !== undefined) {
    const isCursorKey = typeof key === "string" && (
      ARROW_SUFFIX[key as TerminalArrowKey] !== undefined ||
      key === "Home" ||
      key === "End"
    );
    const keyOptions = isCursorKey
      ? { ...options, applicationCursorKeys: data.startsWith("\x1bO") }
      : options;
    return encodeTerminalKey(key, modifiers, keyOptions);
  }

  const characters = Array.from(data);
  if (characters.length === 1) {
    const character = characters[0];
    const effectiveModifiers = modifiers.ctrl &&
      !/^[\x40-\x5f]$/.test(character.toUpperCase())
      ? { ...modifiers, ctrl: false }
      : modifiers;
    return encodeTerminalKey(
      { kind: "character", value: character },
      effectiveModifiers,
      options,
    );
  }

  return {
    data: modifiers.alt && data.length > 0 ? `\x1b${data}` : data,
    nextModifiers: clearModifiers(modifiers),
  };
}
