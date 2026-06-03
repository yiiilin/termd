import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { SessionList } from "./SessionList";

describe("SessionList", () => {
  it("使用真实打开按钮，避免行按钮里嵌套操作按钮", () => {
    render(
      <SessionList
        sessions={[
          {
            session_id: "00000000-0000-0000-0000-000000000401",
            name: "work",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
        ]}
        renameDraft=""
        canSaveRename={false}
        onAttach={vi.fn()}
        onStartRename={vi.fn()}
        onRenameDraftChange={vi.fn()}
        onSaveRename={vi.fn()}
        onCancelRename={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    const openButton = screen.getByRole("button", { name: "Open work" });
    const row = openButton.closest(".session-row");
    expect(openButton.tagName).toBe("BUTTON");
    expect(row).not.toHaveAttribute("role", "button");
    expect(within(openButton).queryByRole("button")).toBeNull();
    expect(within(row as HTMLElement).getByRole("button", { name: "Rename session" })).toBeInTheDocument();
    expect(within(row as HTMLElement).getByRole("button", { name: "Close session" })).toBeInTheDocument();
  });
});
