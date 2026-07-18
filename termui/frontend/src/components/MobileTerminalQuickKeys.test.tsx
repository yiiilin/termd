import { act, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MobileTerminalQuickKeys } from "./MobileTerminalQuickKeys";
import type { ModifierState } from "./terminal/mobile-terminal-keys";

const EMPTY_MODIFIERS: ModifierState = { ctrl: false, alt: false, shift: false };

function QuickKeysHarness(props: Omit<React.ComponentProps<typeof MobileTerminalQuickKeys>, "modifiers" | "onModifiersChange">) {
  const [modifiers, setModifiers] = useState<ModifierState>(EMPTY_MODIFIERS);
  return <MobileTerminalQuickKeys {...props} modifiers={modifiers} onModifiersChange={setModifiers} />;
}

function fireTouchPointer(
  target: HTMLElement,
  type: "pointerdown" | "pointermove" | "pointerup" | "pointercancel",
  input: { pointerId?: number; clientX?: number; clientY?: number } = {},
): Event {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: input.pointerId ?? 1 },
    pointerType: { value: "touch" },
    button: { value: 0 },
    clientX: { value: input.clientX ?? 20 },
    clientY: { value: input.clientY ?? 20 },
  });
  fireEvent(target, event);
  return event;
}

function fireMousePointerDown(target: HTMLElement): Event {
  const event = new Event("pointerdown", { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: 1 },
    pointerType: { value: "mouse" },
    button: { value: 0 },
    clientX: { value: 20 },
    clientY: { value: 20 },
  });
  fireEvent(target, event);
  return event;
}

function tapTouch(target: HTMLElement, pointerId = 1): Event {
  const pointerDown = fireTouchPointer(target, "pointerdown", { pointerId });
  fireTouchPointer(target, "pointerup", { pointerId });
  return pointerDown;
}

function renderQuickKeys(options: {
  applicationCursorKeysMode?: boolean;
  customShortcuts?: Array<{ label: string; data: string }>;
} = {}) {
  const onInput = vi.fn();
  const onPaste = vi.fn();
  const onExpandedChange = vi.fn();
  const onPreserveFocus = vi.fn((event: React.PointerEvent<HTMLButtonElement>) => {
    event.preventDefault();
    event.stopPropagation();
  });
  render(
    <QuickKeysHarness
      customShortcuts={options.customShortcuts}
      getApplicationCursorKeysMode={() => Boolean(options.applicationCursorKeysMode)}
      onInput={onInput}
      onPaste={onPaste}
      onPreserveFocus={onPreserveFocus}
      onExpandedChange={onExpandedChange}
    />,
  );
  return { onInput, onPaste, onExpandedChange, onPreserveFocus };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("MobileTerminalQuickKeys", () => {
  it("renders the compact mobile terminal key order and keeps buttons out of the tab order", () => {
    renderQuickKeys();

    const labels = Array.from(screen.getByTestId("terminal-quick-keys-main").querySelectorAll("button"))
      .map((button) => button.textContent?.trim())
      .filter(Boolean);
    expect(labels.slice(0, 9)).toEqual(["ESC", "TAB", "CTRL", "ALT", "SHIFT", "←", "↑", "↓", "→"]);
    for (const button of screen.getAllByRole("button")) {
      expect(button).toHaveAttribute("tabindex", "-1");
    }
  });

  it("commits an unmoved touch on pointerup and suppresses its synthetic click", () => {
    const { onInput } = renderQuickKeys();
    const escape = screen.getByRole("button", { name: "Escape" });

    const pointerDown = fireTouchPointer(escape, "pointerdown");
    expect(pointerDown.defaultPrevented).toBe(true);
    expect(onInput).not.toHaveBeenCalled();

    fireTouchPointer(escape, "pointerup");
    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onInput).toHaveBeenCalledWith("\x1b");

    fireEvent.click(escape);
    expect(onInput).toHaveBeenCalledTimes(1);
  });

  it("falls back to window pointer handlers when pointer capture is unavailable", () => {
    const { onInput } = renderQuickKeys();
    const escape = screen.getByRole("button", { name: "Escape" });
    Object.defineProperty(escape, "setPointerCapture", {
      configurable: true,
      value: vi.fn(() => {
        throw new DOMException("No active pointer", "NotFoundError");
      }),
    });

    tapTouch(escape);

    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onInput).toHaveBeenCalledWith("\x1b");
  });

  it("uses one-shot sticky modifiers and supports Shift+Tab", () => {
    const { onInput } = renderQuickKeys();
    const ctrl = screen.getByRole("button", { name: "Ctrl" });
    const shift = screen.getByRole("button", { name: "Shift" });
    const tab = screen.getByRole("button", { name: "Tab" });

    tapTouch(ctrl, 1);
    tapTouch(shift, 2);
    expect(ctrl).toHaveAttribute("aria-pressed", "true");
    expect(shift).toHaveAttribute("aria-pressed", "true");

    tapTouch(tab, 3);

    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onInput).toHaveBeenCalledWith("\x1b[Z");
    expect(ctrl).toHaveAttribute("aria-pressed", "false");
    expect(shift).toHaveAttribute("aria-pressed", "false");
  });

  it("reads DECCKM for every arrow press", () => {
    let applicationMode = false;
    const onInput = vi.fn();
    render(
      <QuickKeysHarness
        getApplicationCursorKeysMode={() => applicationMode}
        onInput={onInput}
        onPaste={vi.fn()}
        onPreserveFocus={(event) => event.preventDefault()}
      />,
    );
    const up = screen.getByRole("button", { name: "Arrow up" });

    tapTouch(up);
    applicationMode = true;
    tapTouch(up, 2);

    expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A", "\x1bOA"]);
  });

  it("cancels a pending touch after horizontal movement reaches 8px", () => {
    const { onInput } = renderQuickKeys();
    const escape = screen.getByRole("button", { name: "Escape" });

    fireTouchPointer(escape, "pointerdown", { clientX: 20, clientY: 20 });
    fireTouchPointer(escape, "pointermove", { clientX: 28, clientY: 20 });
    fireTouchPointer(escape, "pointerup", { clientX: 28, clientY: 20 });

    expect(onInput).not.toHaveBeenCalled();
  });

  it("cancels a pending touch on pointercancel before the repeat delay", () => {
    vi.useFakeTimers();
    const { onInput } = renderQuickKeys();
    const up = screen.getByRole("button", { name: "Arrow up" });

    fireTouchPointer(up, "pointerdown");
    act(() => vi.advanceTimersByTime(419));
    fireTouchPointer(up, "pointercancel");
    act(() => vi.advanceTimersByTime(500));

    expect(onInput).not.toHaveBeenCalled();
  });

  it("cancels a pending touch when the quick-key strip scrolls", () => {
    vi.useFakeTimers();
    const { onInput } = renderQuickKeys();
    const up = screen.getByRole("button", { name: "Arrow up" });
    const main = screen.getByTestId("terminal-quick-keys-main");

    fireTouchPointer(up, "pointerdown");
    act(() => vi.advanceTimersByTime(419));
    fireEvent.scroll(main);
    act(() => vi.advanceTimersByTime(500));

    expect(onInput).not.toHaveBeenCalled();
  });

  it("starts arrow repeat at 420ms, repeats every 70ms, and stops on release", () => {
    vi.useFakeTimers();
    let applicationMode = false;
    const onInput = vi.fn();
    render(
      <QuickKeysHarness
        getApplicationCursorKeysMode={() => applicationMode}
        onInput={onInput}
        onPaste={vi.fn()}
        onPreserveFocus={(event) => event.preventDefault()}
      />,
    );
    const shift = screen.getByRole("button", { name: "Shift" });
    const up = screen.getByRole("button", { name: "Arrow up" });

    fireEvent.click(shift);
    fireTouchPointer(up, "pointerdown");
    act(() => vi.advanceTimersByTime(419));
    expect(onInput).not.toHaveBeenCalled();

    act(() => vi.advanceTimersByTime(1));
    expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[1;2A"]);
    act(() => vi.advanceTimersByTime(69));
    expect(onInput).toHaveBeenCalledTimes(1);

    act(() => vi.advanceTimersByTime(1));
    expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[1;2A", "\x1b[A"]);
    applicationMode = true;
    act(() => vi.advanceTimersByTime(70));
    expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[1;2A", "\x1b[A", "\x1bOA"]);

    fireTouchPointer(up, "pointerup");
    act(() => vi.advanceTimersByTime(500));
    expect(onInput).toHaveBeenCalledTimes(3);
  });

  it("activates regular, expansion, panel, and paste actions from click alone", () => {
    const { onInput, onPaste, onExpandedChange } = renderQuickKeys({
      customShortcuts: [{ label: "BREAK", data: "\x1b[19;2~" }],
    });
    const input = document.createElement("textarea");
    document.body.append(input);
    input.focus();

    fireEvent.click(screen.getByRole("button", { name: "Escape" }));
    expect(onInput).toHaveBeenLastCalledWith("\x1b");

    fireEvent.click(screen.getByRole("button", { name: "Expand terminal keys" }));
    expect(onExpandedChange).toHaveBeenLastCalledWith(true);
    for (const label of ["HOME", "END", "PGUP", "PGDN", "INS", "DEL"]) {
      expect(screen.getByRole("button", { name: label })).toBeVisible();
    }

    fireEvent.click(screen.getByRole("tab", { name: "Ctrl combinations" }));
    for (const label of ["^C", "^D", "^Z", "^R", "^L", "^A", "^E", "^W", "^U", "^K"]) {
      expect(screen.getByRole("button", { name: label })).toBeVisible();
    }
    fireEvent.click(screen.getByRole("button", { name: "^C" }));
    expect(onInput).toHaveBeenLastCalledWith("\x03");

    fireEvent.click(screen.getByRole("tab", { name: "Function keys" }));
    expect(screen.getByRole("button", { name: "F1" })).toBeVisible();
    expect(screen.getByRole("button", { name: "F12" })).toBeVisible();

    fireEvent.click(screen.getByRole("tab", { name: "Symbols" }));
    for (const symbol of ["|", "\\", "/", "~", "$", "&", "*", "-", "_", "=", "+", "[", "]", "{", "}", "(", ")", ":", ";", "'", "\"", "<", ">"] ) {
      expect(screen.getByRole("button", { name: symbol })).toBeVisible();
    }
    expect(screen.getByRole("button", { name: "BREAK" })).toBeVisible();

    fireEvent.click(screen.getByRole("tab", { name: "Navigation keys" }));
    fireEvent.click(screen.getByRole("button", { name: "Paste" }));
    expect(onPaste).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole("button", { name: "Collapse terminal keys" }));
    expect(onExpandedChange).toHaveBeenLastCalledWith(false);
    expect(document.activeElement).toBe(input);
  });

  it("uses click, not mouse pointerdown, for mouse activation", () => {
    const { onInput } = renderQuickKeys();
    const escape = screen.getByRole("button", { name: "Escape" });

    fireMousePointerDown(escape);
    expect(onInput).not.toHaveBeenCalled();
    fireEvent.click(escape);

    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onInput).toHaveBeenCalledWith("\x1b");
  });

  it("sends sticky Ctrl with a supported symbol as its control code", () => {
    const { onInput } = renderQuickKeys();

    fireEvent.click(screen.getByRole("button", { name: "Expand terminal keys" }));
    fireEvent.click(screen.getByRole("tab", { name: "Symbols" }));
    fireEvent.click(screen.getByRole("button", { name: "Ctrl" }));
    fireEvent.click(screen.getByRole("button", { name: "\\" }));

    expect(onInput).toHaveBeenLastCalledWith("\x1c");
  });
});
