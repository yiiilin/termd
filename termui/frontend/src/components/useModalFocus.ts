import { useEffect, useRef, type RefObject } from "react";

interface UseModalFocusOptions {
  open: boolean;
  onClose: () => void;
  restoreFocus?: boolean;
}

const focusableSelector = [
  'a[href]:not([tabindex="-1"])',
  'button:not([disabled]):not([tabindex="-1"])',
  'input:not([disabled]):not([tabindex="-1"])',
  'select:not([disabled]):not([tabindex="-1"])',
  'textarea:not([disabled]):not([tabindex="-1"])',
  '[contenteditable="true"]:not([tabindex="-1"])',
  '[tabindex]:not([tabindex="-1"])',
].join(",");

interface ModalFocusEntry {
  dialog: HTMLElement;
}

interface InertSnapshot {
  hadAttribute: boolean;
  value: string | null;
}

const modalFocusStack: ModalFocusEntry[] = [];
const managedInertElements = new Map<HTMLElement, InertSnapshot>();
let focusGuardDocument: Document | undefined;

function focusFirstTarget(dialog: HTMLElement): void {
  const firstFocusable = dialog.querySelector<HTMLElement>(focusableSelector);
  if (firstFocusable) {
    firstFocusable.focus();
    return;
  }
  // Lazy children may not have mounted yet. Keep focus within the modal until
  // its first real control becomes available.
  dialog.tabIndex = -1;
  dialog.focus();
}

function topModalEntry(): ModalFocusEntry | undefined {
  while (modalFocusStack.length > 0 && !modalFocusStack.at(-1)?.dialog.isConnected) {
    modalFocusStack.pop();
  }
  return modalFocusStack.at(-1);
}

function restoreManagedInertElements(): void {
  for (const [element, snapshot] of managedInertElements) {
    if (snapshot.hadAttribute) {
      element.setAttribute("inert", snapshot.value ?? "");
    } else {
      element.removeAttribute("inert");
    }
  }
  managedInertElements.clear();
}

function markInert(element: HTMLElement): void {
  if (!managedInertElements.has(element)) {
    managedInertElements.set(element, {
      hadAttribute: element.hasAttribute("inert"),
      value: element.getAttribute("inert"),
    });
  }
  element.setAttribute("inert", "");
}

function handleDocumentFocusIn(event: FocusEvent): void {
  const topEntry = topModalEntry();
  const target = event.target;
  if (!topEntry || !(target instanceof Node) || topEntry.dialog.contains(target)) {
    return;
  }
  focusFirstTarget(topEntry.dialog);
}

function syncModalStack(document: Document): void {
  restoreManagedInertElements();
  const topEntry = topModalEntry();
  if (!topEntry) {
    focusGuardDocument?.removeEventListener("focusin", handleDocumentFocusIn);
    focusGuardDocument = undefined;
    return;
  }

  let activeBranch: HTMLElement = topEntry.dialog;
  while (activeBranch.parentElement) {
    const parent = activeBranch.parentElement;
    for (const sibling of parent.children) {
      if (sibling !== activeBranch && sibling instanceof HTMLElement) {
        markInert(sibling);
      }
    }
    if (parent === document.body) {
      break;
    }
    activeBranch = parent;
  }

  if (focusGuardDocument !== document) {
    focusGuardDocument?.removeEventListener("focusin", handleDocumentFocusIn);
    document.addEventListener("focusin", handleDocumentFocusIn);
    focusGuardDocument = document;
  }
}

export function useModalFocus<T extends HTMLElement = HTMLElement>({
  open,
  onClose,
  restoreFocus = true,
}: UseModalFocusOptions): RefObject<T | null> {
  const dialogRef = useRef<T>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!open || !dialog) {
      return;
    }

    const previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const stackEntry = { dialog };
    modalFocusStack.push(stackEntry);
    syncModalStack(document);
    focusFirstTarget(dialog);

    const restoreFocusIfNeeded = () => {
      if (topModalEntry()?.dialog !== dialog) {
        return;
      }

      const activeElement = document.activeElement;
      if (
        activeElement === dialog ||
        (activeElement instanceof HTMLElement &&
          dialog.contains(activeElement) &&
          activeElement.matches(focusableSelector))
      ) {
        return;
      }

      focusFirstTarget(dialog);
    };

    const focusTargetObserver = new MutationObserver(restoreFocusIfNeeded);
    focusTargetObserver.observe(dialog, {
      attributes: true,
      attributeFilter: ["disabled"],
      childList: true,
      subtree: true,
    });

    const handleFocusOut = () => {
      queueMicrotask(restoreFocusIfNeeded);
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (topModalEntry()?.dialog !== dialog) {
        return;
      }

      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        onCloseRef.current();
        return;
      }

      if (event.key !== "Tab") {
        return;
      }

      const focusableElements = Array.from(dialog.querySelectorAll<HTMLElement>(focusableSelector));
      const first = focusableElements[0];
      const last = focusableElements.at(-1);
      if (!first || !last) {
        event.preventDefault();
        return;
      }

      if (document.activeElement === dialog) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      } else if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    const handleDocumentKeyDown = (event: KeyboardEvent) => {
      const target = event.target;
      if (target instanceof Node && dialog.contains(target)) {
        return;
      }
      handleKeyDown(event);
    };

    dialog.addEventListener("focusout", handleFocusOut);
    dialog.addEventListener("keydown", handleKeyDown);
    document.addEventListener("keydown", handleDocumentKeyDown);
    return () => {
      focusTargetObserver.disconnect();
      dialog.removeEventListener("focusout", handleFocusOut);
      dialog.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("keydown", handleDocumentKeyDown);
      const stackIndex = modalFocusStack.indexOf(stackEntry);
      if (stackIndex >= 0) {
        modalFocusStack.splice(stackIndex, 1);
      }
      syncModalStack(document);
      if (restoreFocus && previouslyFocused?.isConnected) {
        previouslyFocused.focus();
      }
    };
  }, [open, restoreFocus]);

  return dialogRef;
}
