import { useEffect, useRef, type RefObject } from "react";

interface UseDismissiblePopoverOptions {
  open: boolean;
  onClose: () => void;
  focusFirst?: boolean;
}

interface DismissiblePopoverRefs<Trigger extends HTMLElement, Popover extends HTMLElement> {
  triggerRef: RefObject<Trigger | null>;
  popoverRef: RefObject<Popover | null>;
}

const focusableSelector = [
  "a[href]",
  "button",
  'input:not([type="hidden"])',
  "select",
  "textarea",
  '[contenteditable="true"]',
  "[tabindex]",
].join(",");
// iOS WebKit can dispatch a null-target focusout before its delayed compatibility click.
const INPUT_SETTLE_MS = 1_000;

function firstFocusableElement(container: HTMLElement): HTMLElement | undefined {
  return Array.from(container.querySelectorAll<HTMLElement>(focusableSelector)).find(
    (element) => !element.matches(":disabled") && element.tabIndex >= 0,
  );
}

export function useDismissiblePopover<
  Trigger extends HTMLElement = HTMLButtonElement,
  Popover extends HTMLElement = HTMLElement,
>({
  open,
  onClose,
  focusFirst = true,
}: UseDismissiblePopoverOptions): DismissiblePopoverRefs<Trigger, Popover> {
  const triggerRef = useRef<Trigger>(null);
  const popoverRef = useRef<Popover>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const popover = popoverRef.current;
    if (!open || !popover) {
      return;
    }

    let closeRequested = false;
    let pointerDownOnTrigger = false;
    let pointerDownInPopover = false;
    let activePointerId: number | undefined;
    let touchPressActive = false;
    let clearPointerDownTimer: number | undefined;
    let focusOutCloseTimer: number | undefined;
    let tabNavigationActive = false;
    let clearTabNavigationTimer: number | undefined;

    const clearFocusOutClose = () => {
      if (focusOutCloseTimer !== undefined) {
        window.clearTimeout(focusOutCloseTimer);
        focusOutCloseTimer = undefined;
      }
    };

    const scheduleFocusOutClose = () => {
      clearFocusOutClose();
      focusOutCloseTimer = window.setTimeout(() => {
        focusOutCloseTimer = undefined;
        const activeElement = document.activeElement;
        if (
          activeElement instanceof Node &&
          (popover.contains(activeElement) || Boolean(triggerRef.current?.contains(activeElement)))
        ) {
          return;
        }
        requestClose(false);
      }, INPUT_SETTLE_MS);
    };

    const requestClose = (restoreTriggerFocus: boolean) => {
      if (closeRequested) {
        return;
      }
      closeRequested = true;
      clearFocusOutClose();
      onCloseRef.current();
      if (restoreTriggerFocus && triggerRef.current?.isConnected) {
        triggerRef.current.focus();
      }
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Tab") {
        tabNavigationActive = true;
        if (clearTabNavigationTimer !== undefined) {
          window.clearTimeout(clearTabNavigationTimer);
        }
        clearTabNavigationTimer = window.setTimeout(() => {
          tabNavigationActive = false;
          clearTabNavigationTimer = undefined;
        }, 0);
        return;
      }
      if (event.key !== "Escape") {
        return;
      }
      event.preventDefault();
      requestClose(true);
    };

    const handlePointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) {
        return;
      }

      clearFocusOutClose();
      if (clearPointerDownTimer !== undefined) {
        window.clearTimeout(clearPointerDownTimer);
        clearPointerDownTimer = undefined;
      }
      pointerDownOnTrigger = Boolean(triggerRef.current?.contains(target));
      pointerDownInPopover = popover.contains(target);
      activePointerId = event.pointerId;
      touchPressActive = false;
      if (pointerDownOnTrigger || pointerDownInPopover) {
        return;
      }
      requestClose(false);
    };

    const clearPointerDown = () => {
      if (clearPointerDownTimer !== undefined) {
        window.clearTimeout(clearPointerDownTimer);
      }
      pointerDownOnTrigger = false;
      pointerDownInPopover = false;
      activePointerId = undefined;
      touchPressActive = false;
      clearPointerDownTimer = undefined;
    };

    const scheduleClearPointerDown = () => {
      if (clearPointerDownTimer !== undefined) {
        window.clearTimeout(clearPointerDownTimer);
      }
      clearPointerDownTimer = window.setTimeout(clearPointerDown, INPUT_SETTLE_MS);
    };

    const handlePointerUp = (event: PointerEvent) => {
      if (event.pointerId === activePointerId) {
        scheduleClearPointerDown();
      }
    };

    const handlePointerCancel = (event: PointerEvent) => {
      if (event.pointerId !== activePointerId) {
        return;
      }
      clearPointerDown();
    };

    const handleTouchStart = (event: TouchEvent) => {
      if (activePointerId !== undefined) {
        return;
      }
      const target = event.target;
      if (!(target instanceof Node)) {
        return;
      }

      clearFocusOutClose();
      pointerDownOnTrigger = Boolean(triggerRef.current?.contains(target));
      pointerDownInPopover = popover.contains(target);
      touchPressActive = pointerDownOnTrigger || pointerDownInPopover;
      if (!touchPressActive) {
        requestClose(false);
      }
    };

    const handleTouchEnd = () => {
      if (touchPressActive) {
        scheduleClearPointerDown();
      }
    };

    const handleTouchCancel = () => {
      if (touchPressActive) {
        clearPointerDown();
      }
    };

    const handleClick = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) {
        return;
      }
      if (popover.contains(target) || Boolean(triggerRef.current?.contains(target))) {
        clearFocusOutClose();
        clearPointerDown();
      }
    };

    const handleFocusOut = (event: FocusEvent) => {
      const nextTarget = event.relatedTarget;
      if (nextTarget instanceof Node && popover.contains(nextTarget)) {
        return;
      }
      if (pointerDownOnTrigger && nextTarget instanceof Node && triggerRef.current?.contains(nextTarget)) {
        return;
      }
      if (nextTarget === null) {
        if (tabNavigationActive) {
          requestClose(false);
          return;
        }
        scheduleFocusOutClose();
        return;
      }
      requestClose(false);
    };

    const handleFocusIn = (event: FocusEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) {
        return;
      }
      if (popover.contains(target) || triggerRef.current?.contains(target)) {
        clearFocusOutClose();
        return;
      }
      if (focusOutCloseTimer !== undefined || pointerDownOnTrigger || pointerDownInPopover || touchPressActive) {
        return;
      }
      requestClose(false);
    };

    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("pointerdown", handlePointerDown, true);
    document.addEventListener("pointerup", handlePointerUp, true);
    document.addEventListener("pointercancel", handlePointerCancel, true);
    document.addEventListener("touchstart", handleTouchStart, true);
    document.addEventListener("touchend", handleTouchEnd, true);
    document.addEventListener("touchcancel", handleTouchCancel, true);
    document.addEventListener("click", handleClick, true);
    document.addEventListener("focusin", handleFocusIn);
    popover.addEventListener("focusout", handleFocusOut);

    if (focusFirst) {
      firstFocusableElement(popover)?.focus();
    }

    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("pointerdown", handlePointerDown, true);
      document.removeEventListener("pointerup", handlePointerUp, true);
      document.removeEventListener("pointercancel", handlePointerCancel, true);
      document.removeEventListener("touchstart", handleTouchStart, true);
      document.removeEventListener("touchend", handleTouchEnd, true);
      document.removeEventListener("touchcancel", handleTouchCancel, true);
      document.removeEventListener("click", handleClick, true);
      document.removeEventListener("focusin", handleFocusIn);
      popover.removeEventListener("focusout", handleFocusOut);
      if (clearPointerDownTimer !== undefined) {
        window.clearTimeout(clearPointerDownTimer);
      }
      if (clearTabNavigationTimer !== undefined) {
        window.clearTimeout(clearTabNavigationTimer);
      }
      clearFocusOutClose();
    };
  }, [focusFirst, open]);

  return { triggerRef, popoverRef };
}
