import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { useDismissiblePopover } from "./useDismissiblePopover";

function TestPopover({ focusFirst = true }: { focusFirst?: boolean }) {
  const [open, setOpen] = useState(false);
  const [actionInvoked, setActionInvoked] = useState(false);
  const { triggerRef, popoverRef } = useDismissiblePopover<HTMLButtonElement, HTMLDivElement>({
    open,
    onClose: () => setOpen(false),
    focusFirst,
  });

  return (
    <>
      <button ref={triggerRef} type="button" onClick={() => setOpen((current) => !current)}>
        Toggle menu
      </button>
      {open ? (
        <div ref={popoverRef} role="menu" aria-label="Test menu">
          <button type="button" disabled>
            Disabled action
          </button>
          <button type="button" tabIndex={-1}>
            Skipped action
          </button>
          <button type="button">First action</button>
          <button type="button" onClick={() => setActionInvoked(true)}>Last action</button>
        </div>
      ) : null}
      {actionInvoked ? <output>Action invoked</output> : null}
      <button type="button">Outside action</button>
    </>
  );
}

function ReadOnlyPopover() {
  const [open, setOpen] = useState(false);
  const { triggerRef, popoverRef } = useDismissiblePopover<HTMLButtonElement, HTMLDivElement>({
    open,
    onClose: () => setOpen(false),
    focusFirst: false,
  });

  return (
    <>
      <button ref={triggerRef} type="button" onClick={() => setOpen((current) => !current)}>
        Toggle details
      </button>
      {open ? <div ref={popoverRef} role="region" aria-label="Read-only details">Details</div> : null}
      <button type="button">Next action</button>
    </>
  );
}

describe("useDismissiblePopover", () => {
  it("focuses the first enabled tabbable element when opened", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));

    expect(screen.getByRole("button", { name: "First action" })).toHaveFocus();
  });

  it("can open without moving focus", async () => {
    const user = userEvent.setup();
    render(<TestPopover focusFirst={false} />);

    const trigger = screen.getByRole("button", { name: "Toggle menu" });
    await user.click(trigger);

    expect(trigger).toHaveFocus();
  });

  it("closes on Escape and restores focus to the trigger", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    const trigger = screen.getByRole("button", { name: "Toggle menu" });
    await user.click(trigger);
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("closes on an outside pointer without restoring trigger focus", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const outside = screen.getByRole("button", { name: "Outside action" });
    await user.click(outside);

    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
    expect(outside).toHaveFocus();
  });

  it("closes after Tab or another focus change leaves the popover", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    await user.tab();
    expect(screen.getByRole("button", { name: "Last action" })).toHaveFocus();
    await user.tab();

    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Outside action" })).toHaveFocus();

    rerender(<TestPopover />);
    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    fireEvent.focusOut(screen.getByRole("button", { name: "First action" }), {
      relatedTarget: screen.getByRole("button", { name: "Outside action" }),
    });

    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
  });

  it("closes immediately when Tab produces a null-target focusout", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const firstAction = screen.getByRole("button", { name: "First action" });

    fireEvent.keyDown(firstAction, { key: "Tab" });
    fireEvent.focusOut(firstAction, { relatedTarget: null });

    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
  });

  it("does not close on trigger pointerdown before the trigger click toggles it", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    const trigger = screen.getByRole("button", { name: "Toggle menu" });
    await user.click(trigger);
    await user.pointer({ target: trigger, keys: "[MouseLeft>]" });

    expect(screen.getByRole("menu", { name: "Test menu" })).toBeInTheDocument();

    await user.pointer({ target: trigger, keys: "[/MouseLeft]" });
    expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
  });

  it("keeps a touch action mounted through a null-target focusout", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const firstAction = screen.getByRole("button", { name: "First action" });
    const lastAction = screen.getByRole("button", { name: "Last action" });

    fireEvent.pointerDown(lastAction, { pointerId: 1, pointerType: "touch" });
    fireEvent.focusOut(firstAction, { relatedTarget: null });

    expect(screen.getByRole("menu", { name: "Test menu" })).toBeInTheDocument();
    fireEvent.pointerUp(lastAction, { pointerId: 1, pointerType: "touch" });
    fireEvent.click(lastAction);
    expect(screen.getByText("Action invoked")).toBeInTheDocument();
  });

  it("keeps an iOS-style focusout that arrives before touch tracking through the action click", async () => {
    vi.useFakeTimers();
    try {
      render(<TestPopover />);

      fireEvent.click(screen.getByRole("button", { name: "Toggle menu" }));
      const firstAction = screen.getByRole("button", { name: "First action" });
      const lastAction = screen.getByRole("button", { name: "Last action" });

      fireEvent.focusOut(firstAction, { relatedTarget: null });
      fireEvent.pointerDown(lastAction, { pointerId: 1, pointerType: "touch" });
      fireEvent.pointerUp(lastAction, { pointerId: 1, pointerType: "touch" });
      act(() => vi.advanceTimersByTime(10));
      fireEvent.click(lastAction);

      expect(screen.getByText("Action invoked")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps an iOS-style delayed null focusout through a compatibility click", async () => {
    vi.useFakeTimers();
    try {
      render(<TestPopover />);

      fireEvent.click(screen.getByRole("button", { name: "Toggle menu" }));
      const firstAction = screen.getByRole("button", { name: "First action" });
      const lastAction = screen.getByRole("button", { name: "Last action" });

      fireEvent.pointerDown(lastAction, { pointerId: 1, pointerType: "touch" });
      fireEvent.pointerUp(lastAction, { pointerId: 1, pointerType: "touch" });
      act(() => vi.advanceTimersByTime(10));
      fireEvent.focusOut(firstAction, { relatedTarget: null });
      act(() => vi.advanceTimersByTime(900));
      fireEvent.click(lastAction);
      act(() => vi.advanceTimersByTime(1_000));

      expect(screen.getByText("Action invoked")).toBeInTheDocument();
      expect(screen.getByRole("menu", { name: "Test menu" })).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps the touch guard for the matching pointer until its click runs", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const firstAction = screen.getByRole("button", { name: "First action" });
    const lastAction = screen.getByRole("button", { name: "Last action" });

    fireEvent.pointerDown(lastAction, { pointerId: 1, pointerType: "touch" });
    fireEvent.focusOut(firstAction, { relatedTarget: null });
    fireEvent.pointerUp(lastAction, { pointerId: 2, pointerType: "touch" });
    fireEvent.click(lastAction);

    expect(screen.getByText("Action invoked")).toBeInTheDocument();
  });

  it("keeps a canceled touch action mounted until its compatibility click runs", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const firstAction = screen.getByRole("button", { name: "First action" });
    const lastAction = screen.getByRole("button", { name: "Last action" });

    fireEvent.pointerDown(lastAction, { pointerId: 1, pointerType: "touch" });
    fireEvent.focusOut(firstAction, { relatedTarget: null });
    fireEvent.pointerCancel(lastAction, { pointerId: 1, pointerType: "touch" });
    fireEvent.click(lastAction);

    expect(screen.getByText("Action invoked")).toBeInTheDocument();
  });

  it("keeps a touch-only action mounted through a null-target focusout", async () => {
    const user = userEvent.setup();
    render(<TestPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle menu" }));
    const firstAction = screen.getByRole("button", { name: "First action" });
    const lastAction = screen.getByRole("button", { name: "Last action" });

    fireEvent.touchStart(lastAction, { touches: [{ identifier: 1 }] });
    fireEvent.focusOut(firstAction, { relatedTarget: null });
    fireEvent.touchEnd(lastAction, { changedTouches: [{ identifier: 1 }] });
    fireEvent.click(lastAction);

    expect(screen.getByText("Action invoked")).toBeInTheDocument();
  });

  it("closes after a touch focus loss settles without a compatibility click", async () => {
    vi.useFakeTimers();
    try {
      render(<TestPopover />);

      fireEvent.click(screen.getByRole("button", { name: "Toggle menu" }));
      const firstAction = screen.getByRole("button", { name: "First action" });
      const lastAction = screen.getByRole("button", { name: "Last action" });

      fireEvent.touchStart(lastAction, { touches: [{ identifier: 1 }] });
      firstAction.blur();
      fireEvent.touchCancel(lastAction, { changedTouches: [{ identifier: 1 }] });
      act(() => vi.advanceTimersByTime(1_000));

      expect(screen.queryByRole("menu", { name: "Test menu" })).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("closes a read-only popover when Tab moves focus beyond its trigger", async () => {
    const user = userEvent.setup();
    render(<ReadOnlyPopover />);

    await user.click(screen.getByRole("button", { name: "Toggle details" }));
    expect(screen.getByRole("region", { name: "Read-only details" })).toBeInTheDocument();
    await user.tab();

    expect(screen.queryByRole("region", { name: "Read-only details" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Next action" })).toHaveFocus();
  });
});
