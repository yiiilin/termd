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

    const requestClose = (restoreTriggerFocus: boolean) => {
      if (closeRequested) {
        return;
      }
      closeRequested = true;
      onCloseRef.current();
      if (restoreTriggerFocus && triggerRef.current?.isConnected) {
        triggerRef.current.focus();
      }
    };

    const handleKeyDown = (event: KeyboardEvent) => {
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

      pointerDownOnTrigger = Boolean(triggerRef.current?.contains(target));
      if (pointerDownOnTrigger || popover.contains(target)) {
        return;
      }
      requestClose(false);
    };

    const clearTriggerPointerDown = () => {
      pointerDownOnTrigger = false;
    };

    const handleFocusOut = (event: FocusEvent) => {
      const nextTarget = event.relatedTarget;
      if (nextTarget instanceof Node && popover.contains(nextTarget)) {
        return;
      }
      if (pointerDownOnTrigger && nextTarget instanceof Node && triggerRef.current?.contains(nextTarget)) {
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
        return;
      }
      requestClose(false);
    };

    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("pointerdown", handlePointerDown, true);
    document.addEventListener("pointerup", clearTriggerPointerDown, true);
    document.addEventListener("pointercancel", clearTriggerPointerDown, true);
    document.addEventListener("focusin", handleFocusIn);
    popover.addEventListener("focusout", handleFocusOut);

    if (focusFirst) {
      firstFocusableElement(popover)?.focus();
    }

    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("pointerdown", handlePointerDown, true);
      document.removeEventListener("pointerup", clearTriggerPointerDown, true);
      document.removeEventListener("pointercancel", clearTriggerPointerDown, true);
      document.removeEventListener("focusin", handleFocusIn);
      popover.removeEventListener("focusout", handleFocusOut);
    };
  }, [focusFirst, open]);

  return { triggerRef, popoverRef };
}
