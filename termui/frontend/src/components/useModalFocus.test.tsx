import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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

function InvalidatingActionDialog() {
  const [open, setOpen] = useState(false);
  const [applied, setApplied] = useState(false);
  const [removed, setRemoved] = useState(false);
  const dialogRef = useModalFocus({ open, onClose: () => setOpen(false) });

  return (
    <>
      <button
        type="button"
        onClick={() => {
          setApplied(false);
          setRemoved(false);
          setOpen(true);
        }}
      >
        Open invalidating dialog
      </button>
      {open ? (
        <section ref={dialogRef} role="dialog" aria-label="Invalidating dialog">
          <button type="button">Fallback action</button>
          <button
            type="button"
            disabled={applied}
            onClick={(event) => {
              setApplied(true);
              event.currentTarget.blur();
            }}
          >
            Apply
          </button>
          {removed ? null : (
            <button type="button" onClick={() => setRemoved(true)}>
              Remove action
            </button>
          )}
        </section>
      ) : null}
    </>
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

  it("pulls focus back into the dialog when a control blurs to the document body", async () => {
    const user = userEvent.setup();
    render(<TestDialog />);

    await user.click(screen.getByRole("button", { name: "Open dialog" }));
    const firstAction = screen.getByRole("button", { name: "First action" });
    firstAction.blur();
    expect(document.body).toHaveFocus();

    await waitFor(() => expect(firstAction).toHaveFocus());
  });

  it("restores focus inside the dialog when the focused control becomes disabled", async () => {
    const user = userEvent.setup();
    render(<InvalidatingActionDialog />);

    await user.click(screen.getByRole("button", { name: "Open invalidating dialog" }));
    await user.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => expect(screen.getByRole("button", { name: "Fallback action" })).toHaveFocus());
  });

  it("restores focus inside the dialog when the focused control is removed", async () => {
    const user = userEvent.setup();
    render(<InvalidatingActionDialog />);

    await user.click(screen.getByRole("button", { name: "Open invalidating dialog" }));
    await user.click(screen.getByRole("button", { name: "Remove action" }));

    await waitFor(() => expect(screen.getByRole("button", { name: "Fallback action" })).toHaveFocus());
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

  it("closes only the top dialog on Escape when focus has fallen back to the document body", async () => {
    const user = userEvent.setup();
    render(<StackedDialogs />);

    const topTrigger = screen.getByRole("button", { name: "Open top dialog" });
    await user.click(topTrigger);
    screen.getByTestId("top-action").blur();
    expect(document.body).toHaveFocus();

    fireEvent.keyDown(document.body, { key: "Escape" });

    expect(screen.queryByRole("alertdialog", { name: "Top dialog" })).not.toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Lower dialog" })).toBeInTheDocument();
    expect(topTrigger).toHaveFocus();
    expect(screen.getByTestId("background-action")).toHaveAttribute("inert");
  });
});
