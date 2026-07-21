import { useState } from "react";
import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { DestructiveActionDialog } from "./DestructiveActionDialog";

const defaultProps = {
  open: true,
  title: "Delete directory?",
  description: "This cannot be undone.",
  target: "/home/alice/projects/a-very-long-directory-name",
  cancelLabel: "Cancel",
  confirmLabel: "Delete",
  busyLabel: "Deleting",
  onCancel: vi.fn(),
  onConfirm: vi.fn(),
};

describe("DestructiveActionDialog", () => {
  it("shows the complete target, focuses Cancel first, and traps Tab", async () => {
    const user = userEvent.setup();
    render(<DestructiveActionDialog {...defaultProps} />);

    const dialog = screen.getByRole("alertdialog", { name: "Delete directory?" });
    expect(within(dialog).getByText(defaultProps.description)).toBeInTheDocument();
    expect(within(dialog).getByText(defaultProps.target)).toHaveAttribute("title", defaultProps.target);

    const cancel = within(dialog).getByRole("button", { name: "Cancel" });
    const confirm = within(dialog).getByRole("button", { name: "Delete" });
    expect(cancel).toHaveFocus();
    await user.tab();
    expect(confirm).toHaveFocus();
    await user.tab();
    expect(cancel).toHaveFocus();
    await user.tab({ shift: true });
    expect(confirm).toHaveFocus();
  });

  it.each(["Cancel", "Escape", "backdrop"])("%s cancels without confirming", async (method) => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    render(<DestructiveActionDialog {...defaultProps} onCancel={onCancel} onConfirm={onConfirm} />);

    const dialog = screen.getByRole("alertdialog");
    if (method === "Cancel") {
      await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    } else if (method === "Escape") {
      fireEvent.keyDown(dialog, { key: "Escape" });
    } else {
      fireEvent.mouseDown(dialog.parentElement as HTMLElement);
    }

    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("runs Confirm only once even before busy is rendered", () => {
    const onConfirm = vi.fn();
    render(<DestructiveActionDialog {...defaultProps} onConfirm={onConfirm} />);

    const confirm = screen.getByRole("button", { name: "Delete" });
    fireEvent.click(confirm);
    fireEvent.click(confirm);

    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("blocks confirm, cancel, Escape, and backdrop while busy", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    render(
      <DestructiveActionDialog
        {...defaultProps}
        busy
        onCancel={onCancel}
        onConfirm={onConfirm}
      />,
    );

    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveAttribute("aria-busy", "true");
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Deleting" })).toBeDisabled();
    await user.click(within(dialog).getByRole("button", { name: "Deleting" }));
    fireEvent.keyDown(dialog, { key: "Escape" });
    fireEvent.mouseDown(dialog.parentElement as HTMLElement);

    expect(onCancel).not.toHaveBeenCalled();
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("restores focus to the opener after Cancel closes it", async () => {
    const user = userEvent.setup();

    function Harness() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setOpen(true)}>Open danger dialog</button>
          <DestructiveActionDialog
            {...defaultProps}
            open={open}
            onCancel={() => setOpen(false)}
          />
        </>
      );
    }

    render(<Harness />);
    const opener = screen.getByRole("button", { name: "Open danger dialog" });
    await user.click(opener);
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(opener).toHaveFocus();
  });
});
