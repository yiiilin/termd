import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { useModalFocus } from "./useModalFocus";

function TestDialog() {
  const [open, setOpen] = useState(false);
  const dialogRef = useModalFocus({ open, onClose: () => setOpen(false) });

  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        Open dialog
      </button>
      {open ? (
        <section ref={dialogRef} role="dialog" aria-label="Test dialog">
          <button type="button">First action</button>
          <button type="button">Last action</button>
        </section>
      ) : null}
    </>
  );
}

function DeferredDialog({ ready }: { ready: boolean }) {
  const dialogRef = useModalFocus({ open: true, onClose: () => undefined });

  return (
    <section ref={dialogRef} role="dialog" aria-label="Deferred dialog">
      {ready ? <button type="button">Deferred action</button> : <span>Loading</span>}
    </section>
  );
}

function StackedDialogs() {
  const [topOpen, setTopOpen] = useState(false);
  const lowerDialogRef = useModalFocus({ open: true, onClose: () => undefined });
  const topDialogRef = useModalFocus({ open: topOpen, onClose: () => setTopOpen(false) });

  return (
    <div>
      <button type="button" data-testid="background-action">
        Background action
      </button>
      <div data-testid="lower-layer">
        <section ref={lowerDialogRef} role="dialog" aria-label="Lower dialog">
          <button type="button" onClick={() => setTopOpen(true)}>
            Open top dialog
          </button>
          <button type="button" data-testid="lower-action">
            Lower action
          </button>
        </section>
      </div>
      {topOpen ? (
        <div data-testid="top-layer">
          <section ref={topDialogRef} role="alertdialog" aria-label="Top dialog">
            <button type="button" data-testid="top-action">
              Top action
            </button>
            <button type="button" onClick={() => setTopOpen(false)}>
              Close top dialog
            </button>
          </section>
        </div>
      ) : null}
    </div>
  );
}

describe("useModalFocus", () => {
  it("moves focus into the dialog when it opens", async () => {
    const user = userEvent.setup();
    render(<TestDialog />);

    await user.click(screen.getByRole("button", { name: "Open dialog" }));

    expect(screen.getByRole("button", { name: "First action" })).toHaveFocus();
  });

  it("keeps forward and backward tab navigation inside the dialog", async () => {
    const user = userEvent.setup();
    render(<TestDialog />);

    await user.click(screen.getByRole("button", { name: "Open dialog" }));
    await user.tab({ shift: true });
    expect(screen.getByRole("button", { name: "Last action" })).toHaveFocus();

    await user.tab();
    expect(screen.getByRole("button", { name: "First action" })).toHaveFocus();
  });

  it("requests close when Escape is pressed", async () => {
    const user = userEvent.setup();
    render(<TestDialog />);

    await user.click(screen.getByRole("button", { name: "Open dialog" }));
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Test dialog" })).not.toBeInTheDocument();
  });

  it("restores focus to the trigger after the dialog closes", async () => {
    const user = userEvent.setup();
    render(<TestDialog />);

    const trigger = screen.getByRole("button", { name: "Open dialog" });
    await user.click(trigger);
    await user.keyboard("{Escape}");

    expect(trigger).toHaveFocus();
  });

  it("keeps focus on a lazy dialog container until a real control mounts", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<DeferredDialog ready={false} />);

    const dialog = screen.getByRole("dialog", { name: "Deferred dialog" });
    expect(dialog).toHaveFocus();

    rerender(<DeferredDialog ready />);
    await user.keyboard("{Tab}");
    expect(screen.getByRole("button", { name: "Deferred action" })).toHaveFocus();
  });

  it("makes every branch below the top modal inert and pulls escaped focus back", async () => {
    const user = userEvent.setup();
    render(<StackedDialogs />);

    await user.click(screen.getByRole("button", { name: "Open top dialog" }));

    expect(screen.getByTestId("background-action")).toHaveAttribute("inert");
    expect(screen.getByTestId("lower-layer")).toHaveAttribute("inert");
    expect(screen.getByTestId("top-layer")).not.toHaveAttribute("inert");
    expect(screen.getByTestId("top-action")).toHaveFocus();

    screen.getByTestId("lower-action").focus();
    expect(screen.getByTestId("top-action")).toHaveFocus();

    await user.click(screen.getByRole("button", { name: "Close top dialog" }));
    expect(screen.getByTestId("lower-layer")).not.toHaveAttribute("inert");
    expect(screen.getByTestId("background-action")).toHaveAttribute("inert");
  });
});
