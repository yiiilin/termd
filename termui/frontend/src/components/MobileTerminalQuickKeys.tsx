import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { ChevronDown, ChevronUp, ClipboardPaste } from "lucide-react";
import type { BrowserMobileShortcut } from "../protocol/types";
import { useI18n } from "../i18n";
import {
  encodeTerminalKey,
  toggleModifier,
  type ModifierKey,
  type ModifierState,
  type TerminalKey,
} from "./terminal/mobile-terminal-keys";

const REPEAT_DELAY_MS = 420;
const REPEAT_INTERVAL_MS = 70;
const REPEAT_SCROLL_CANCEL_PX = 8;
const SYNTHETIC_CLICK_SUPPRESSION_MS = 750;

const EMPTY_MODIFIERS: ModifierState = { ctrl: false, alt: false, shift: false };
const CTRL_KEYS = ["C", "D", "Z", "R", "L", "A", "E", "W", "U", "K"] as const;
const FUNCTION_KEYS = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"] as const;
const SYMBOL_KEYS = [
  "|", "\\", "/", "~", "$", "&", "*", "-", "_", "=", "+",
  "[", "]", "{", "}", "(", ")", ":", ";", "'", "\"", "<", ">",
] as const;

type QuickKeysPanel = "navigation" | "ctrl" | "function" | "symbols";

interface ActivePointerAction {
  pointerId: number;
  startX: number;
  startY: number;
  target: HTMLButtonElement;
  action: () => void;
  repeat?: () => void;
  timeoutId?: number;
  intervalId?: number;
  fired: boolean;
}

interface SuppressedClick {
  target: HTMLButtonElement;
  timeoutId: number;
}

interface MobileTerminalQuickKeysProps {
  customShortcuts?: BrowserMobileShortcut[];
  getApplicationCursorKeysMode: () => boolean;
  modifiers: ModifierState;
  onModifiersChange: (modifiers: ModifierState) => void;
  onInput: (data: string) => void;
  onPaste: () => void;
  onPreserveFocus: (event: ReactPointerEvent<HTMLButtonElement>) => void;
  onExpandedChange?: (expanded: boolean) => void;
}

interface QuickKeyButtonProps {
  ariaLabel: string;
  children: ReactNode;
  active?: boolean;
  className?: string;
  repeat?: boolean;
  onPress: () => void;
  onPointerStart: (
    event: ReactPointerEvent<HTMLButtonElement>,
    action: () => void,
    repeat: boolean,
  ) => void;
  onClickAction: (
    event: ReactMouseEvent<HTMLButtonElement>,
    action: () => void,
  ) => void;
  onPreserveFocus: (event: ReactPointerEvent<HTMLButtonElement>) => void;
}

export function MobileTerminalQuickKeys(props: MobileTerminalQuickKeysProps) {
  const { t } = useI18n();
  const [expanded, setExpanded] = useState(false);
  const [panel, setPanel] = useState<QuickKeysPanel>("navigation");
  const activePointerActionRef = useRef<ActivePointerAction | undefined>(undefined);
  const suppressedClickRef = useRef<SuppressedClick | undefined>(undefined);

  const clearActivePointerAction = useCallback(() => {
    const active = activePointerActionRef.current;
    if (!active) {
      return undefined;
    }
    if (active.timeoutId !== undefined) {
      window.clearTimeout(active.timeoutId);
    }
    if (active.intervalId !== undefined) {
      window.clearInterval(active.intervalId);
    }
    try {
      if (active.target.hasPointerCapture?.(active.pointerId)) {
        active.target.releasePointerCapture?.(active.pointerId);
      }
    } catch {
      // The window-level pointer handlers still finish the gesture when capture is unavailable.
    }
    activePointerActionRef.current = undefined;
    return active;
  }, []);

  const clearSuppressedClick = useCallback(() => {
    const suppressed = suppressedClickRef.current;
    if (!suppressed) {
      return;
    }
    window.clearTimeout(suppressed.timeoutId);
    suppressedClickRef.current = undefined;
  }, []);

  const suppressNextClick = useCallback((target: HTMLButtonElement) => {
    clearSuppressedClick();
    const suppressed: SuppressedClick = {
      target,
      timeoutId: window.setTimeout(() => {
        if (suppressedClickRef.current === suppressed) {
          suppressedClickRef.current = undefined;
        }
      }, SYNTHETIC_CLICK_SUPPRESSION_MS),
    };
    suppressedClickRef.current = suppressed;
  }, [clearSuppressedClick]);

  const cancelPointerAction = useCallback((pointerId?: number) => {
    const active = activePointerActionRef.current;
    if (!active || (pointerId !== undefined && active.pointerId !== pointerId)) {
      return;
    }
    clearActivePointerAction();
    suppressNextClick(active.target);
  }, [clearActivePointerAction, suppressNextClick]);

  const finishPointerAction = useCallback((pointerId: number) => {
    const active = activePointerActionRef.current;
    if (!active || active.pointerId !== pointerId) {
      return;
    }
    clearActivePointerAction();
    suppressNextClick(active.target);
    if (!active.fired) {
      active.action();
    }
  }, [clearActivePointerAction, suppressNextClick]);

  useEffect(() => {
    const finish = (event: PointerEvent) => finishPointerAction(event.pointerId);
    const cancel = (event: PointerEvent) => cancelPointerAction(event.pointerId);
    const cancelAny = () => cancelPointerAction();
    window.addEventListener("pointerup", finish, true);
    window.addEventListener("pointercancel", cancel, true);
    window.addEventListener("blur", cancelAny);
    document.addEventListener("visibilitychange", cancelAny);
    return () => {
      clearActivePointerAction();
      clearSuppressedClick();
      window.removeEventListener("pointerup", finish, true);
      window.removeEventListener("pointercancel", cancel, true);
      window.removeEventListener("blur", cancelAny);
      document.removeEventListener("visibilitychange", cancelAny);
    };
  }, [cancelPointerAction, clearActivePointerAction, clearSuppressedClick, finishPointerAction]);

  const sendKey = (key: TerminalKey, forcedModifiers?: Partial<ModifierState>) => {
    const effectiveModifiers = forcedModifiers
      ? { ...props.modifiers, ...forcedModifiers }
      : props.modifiers;
    const encoded = encodeTerminalKey(key, effectiveModifiers, {
      applicationCursorKeys: props.getApplicationCursorKeysMode(),
    });
    props.onModifiersChange(encoded.nextModifiers);
    props.onInput(encoded.data);
  };

  const sendCharacter = (character: string, forcedModifiers?: Partial<ModifierState>) => {
    let effectiveModifiers = forcedModifiers
      ? { ...props.modifiers, ...forcedModifiers }
      : props.modifiers;
    if (effectiveModifiers.ctrl && !/^[\x40-\x5f]$/.test(character.toUpperCase())) {
      effectiveModifiers = { ...effectiveModifiers, ctrl: false };
    }
    const encoded = encodeTerminalKey({ kind: "character", value: character }, effectiveModifiers, {
      applicationCursorKeys: props.getApplicationCursorKeysMode(),
    });
    props.onModifiersChange(encoded.nextModifiers);
    props.onInput(encoded.data);
  };

  const sendCustomShortcut = (shortcut: BrowserMobileShortcut) => {
    if ([...shortcut.data].length === 1) {
      sendCharacter(shortcut.data);
      return;
    }
    props.onModifiersChange(EMPTY_MODIFIERS);
    props.onInput(shortcut.data);
  };

  const startPointerAction = (
    event: ReactPointerEvent<HTMLButtonElement>,
    action: () => void,
    shouldRepeat: boolean,
  ) => {
    cancelPointerAction();
    if (event.pointerType !== "touch" || event.button !== 0) {
      return;
    }
    try {
      event.currentTarget.setPointerCapture?.(event.pointerId);
    } catch {
      // Synthetic and accessibility-dispatched PointerEvents may not be registered as active pointers.
    }
    const repeatKey = event.currentTarget.dataset.terminalKey as TerminalKey | undefined;
    const repeat = shouldRepeat && repeatKey ? () => {
      const key = repeatKey;
      const encoded = encodeTerminalKey(key, EMPTY_MODIFIERS, {
        applicationCursorKeys: props.getApplicationCursorKeysMode(),
      });
      props.onInput(encoded.data);
    } : undefined;
    const active: ActivePointerAction = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      target: event.currentTarget,
      action,
      repeat,
      fired: false,
    };
    if (repeat) {
      active.timeoutId = window.setTimeout(() => {
        if (activePointerActionRef.current !== active) {
          return;
        }
        active.timeoutId = undefined;
        active.fired = true;
        active.action();
        active.intervalId = window.setInterval(repeat, REPEAT_INTERVAL_MS);
      }, REPEAT_DELAY_MS);
    }
    activePointerActionRef.current = active;
  };

  const handlePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const active = activePointerActionRef.current;
    if (!active || active.pointerId !== event.pointerId) {
      return;
    }
    if (
      Math.abs(event.clientX - active.startX) >= REPEAT_SCROLL_CANCEL_PX ||
      Math.abs(event.clientY - active.startY) >= REPEAT_SCROLL_CANCEL_PX
    ) {
      cancelPointerAction(event.pointerId);
    }
  };

  const handleClickAction = (
    event: ReactMouseEvent<HTMLButtonElement>,
    action: () => void,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    if (suppressedClickRef.current?.target === event.currentTarget) {
      clearSuppressedClick();
      return;
    }
    action();
  };

  const toggleExpanded = () => {
    const next = !expanded;
    setExpanded(next);
    if (next) {
      setPanel("navigation");
    }
    props.onExpandedChange?.(next);
  };

  const toggleStickyModifier = (modifier: ModifierKey) => {
    props.onModifiersChange(toggleModifier(props.modifiers, modifier));
  };

  const arrowLabels: Record<"ArrowLeft" | "ArrowUp" | "ArrowDown" | "ArrowRight", string> = {
    ArrowLeft: t("terminal.quickKeys.arrowLeft"),
    ArrowUp: t("terminal.quickKeys.arrowUp"),
    ArrowDown: t("terminal.quickKeys.arrowDown"),
    ArrowRight: t("terminal.quickKeys.arrowRight"),
  };

  return (
    <div
      className={`terminal-mobile-shortcuts${expanded ? " expanded" : ""}`}
      aria-label={t("terminal.mobileShortcuts")}
      onPointerMove={handlePointerMove}
      onPointerCancel={(event) => cancelPointerAction(event.pointerId)}
      onScrollCapture={() => cancelPointerAction()}
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
      }}
    >
      {expanded ? (
        <div className="terminal-quick-keys-panel">
          <div className="terminal-quick-keys-tabs" role="tablist" aria-label={t("terminal.mobileShortcuts")}>
            <PanelTab label="NAV" ariaLabel={t("terminal.quickKeys.navigation")} selected={panel === "navigation"} onPress={() => setPanel("navigation")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus} />
            <PanelTab label="CTRL" ariaLabel={t("terminal.quickKeys.ctrl")} selected={panel === "ctrl"} onPress={() => setPanel("ctrl")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus} />
            <PanelTab label="FN" ariaLabel={t("terminal.quickKeys.function")} selected={panel === "function"} onPress={() => setPanel("function")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus} />
            <PanelTab label="SYM" ariaLabel={t("terminal.quickKeys.symbols")} selected={panel === "symbols"} onPress={() => setPanel("symbols")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus} />
          </div>
          <div className="terminal-quick-keys-panel-strip" onScroll={() => cancelPointerAction()}>
            {panel === "navigation" ? (
              <>
                {([
                  ["HOME", "Home"], ["END", "End"], ["PGUP", "PgUp"],
                  ["PGDN", "PgDn"], ["INS", "Insert"], ["DEL", "Delete"],
                ] as const).map(([label, key]) => (
                  <QuickKeyButton key={key} ariaLabel={label} onPress={() => sendKey(key)} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>{label}</QuickKeyButton>
                ))}
                <QuickKeyButton ariaLabel={t("terminal.paste")} className="icon paste" onPress={props.onPaste} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>
                  <ClipboardPaste size={15} aria-hidden="true" />
                </QuickKeyButton>
              </>
            ) : null}
            {panel === "ctrl" ? CTRL_KEYS.map((letter) => (
              <QuickKeyButton key={letter} ariaLabel={`^${letter}`} onPress={() => sendCharacter(letter, { ctrl: true })} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>{`^${letter}`}</QuickKeyButton>
            )) : null}
            {panel === "function" ? FUNCTION_KEYS.map((key) => (
              <QuickKeyButton key={key} ariaLabel={key} onPress={() => sendKey(key)} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>{key}</QuickKeyButton>
            )) : null}
            {panel === "symbols" ? (
              <>
                {SYMBOL_KEYS.map((symbol) => (
                  <QuickKeyButton key={symbol} ariaLabel={symbol} onPress={() => sendCharacter(symbol)} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>{symbol}</QuickKeyButton>
                ))}
                {(props.customShortcuts ?? []).map((shortcut, index) => (
                  <QuickKeyButton key={`${shortcut.label}:${index}`} ariaLabel={shortcut.label} className="custom" onPress={() => sendCustomShortcut(shortcut)} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>{shortcut.label}</QuickKeyButton>
                ))}
              </>
            ) : null}
          </div>
        </div>
      ) : null}

      <div className="terminal-quick-keys-main" data-testid="terminal-quick-keys-main" onScroll={() => cancelPointerAction()}>
        <QuickKeyButton ariaLabel="Escape" onPress={() => sendKey("Esc")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>ESC</QuickKeyButton>
        <QuickKeyButton ariaLabel="Tab" onPress={() => sendKey("Tab")} onPointerStart={startPointerAction} onClickAction={handleClickAction} onPreserveFocus={props.onPreserveFocus}>TAB</QuickKeyButton>
        {(["ctrl", "alt", "shift"] as const).map((modifier) => (
          <QuickKeyButton
            key={modifier}
            ariaLabel={modifier === "ctrl" ? "Ctrl" : modifier === "alt" ? "Alt" : "Shift"}
            active={props.modifiers[modifier]}
            className={`modifier-${modifier}`}
            onPress={() => toggleStickyModifier(modifier)}
            onPointerStart={startPointerAction}
            onClickAction={handleClickAction}
            onPreserveFocus={props.onPreserveFocus}
          >
            {modifier.toUpperCase()}
          </QuickKeyButton>
        ))}
        {(["ArrowLeft", "ArrowUp", "ArrowDown", "ArrowRight"] as const).map((key) => (
          <QuickKeyButton
            key={key}
            ariaLabel={arrowLabels[key]}
            repeat
            onPress={() => sendKey(key)}
            onPointerStart={(event, action, repeat) => {
              event.currentTarget.dataset.terminalKey = key;
              startPointerAction(event, action, repeat);
            }}
            onClickAction={handleClickAction}
            onPreserveFocus={props.onPreserveFocus}
          >
            {key === "ArrowLeft" ? "←" : key === "ArrowUp" ? "↑" : key === "ArrowDown" ? "↓" : "→"}
          </QuickKeyButton>
        ))}
        <QuickKeyButton
          ariaLabel={expanded ? t("terminal.quickKeys.collapse") : t("terminal.quickKeys.expand")}
          className="icon"
          onPress={toggleExpanded}
          onPointerStart={startPointerAction}
          onClickAction={handleClickAction}
          onPreserveFocus={props.onPreserveFocus}
        >
          {expanded ? <ChevronDown size={16} aria-hidden="true" /> : <ChevronUp size={16} aria-hidden="true" />}
        </QuickKeyButton>
      </div>
    </div>
  );
}

function QuickKeyButton(props: QuickKeyButtonProps) {
  return (
    <button
      type="button"
      tabIndex={-1}
      className={`terminal-mobile-shortcut-button${props.active ? " active" : ""}${props.className ? ` ${props.className}` : ""}`}
      aria-label={props.ariaLabel}
      aria-pressed={props.active === undefined ? undefined : props.active}
      title={props.ariaLabel}
      onPointerDown={(event) => {
        props.onPreserveFocus(event);
        props.onPointerStart(event, props.onPress, Boolean(props.repeat));
      }}
      onClick={(event) => props.onClickAction(event, props.onPress)}
    >
      {props.children}
    </button>
  );
}

function PanelTab(props: {
  label: string;
  ariaLabel: string;
  selected: boolean;
  onPress: () => void;
  onPointerStart: QuickKeyButtonProps["onPointerStart"];
  onClickAction: QuickKeyButtonProps["onClickAction"];
  onPreserveFocus: QuickKeyButtonProps["onPreserveFocus"];
}) {
  return (
    <button
      type="button"
      role="tab"
      tabIndex={-1}
      aria-label={props.ariaLabel}
      aria-selected={props.selected}
      className="terminal-quick-keys-tab"
      onPointerDown={(event) => {
        props.onPreserveFocus(event);
        props.onPointerStart(event, props.onPress, false);
      }}
      onClick={(event) => props.onClickAction(event, props.onPress)}
    >
      {props.label}
    </button>
  );
}
