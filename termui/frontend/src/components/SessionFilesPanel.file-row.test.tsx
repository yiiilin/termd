import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SessionFilesPanel } from "./SessionFilesPanel";
import { useModalFocus } from "./useModalFocus";

const sessionId = "00000000-0000-0000-0000-0000000004f2";
const directoryEntry = {
  name: "src",
  path: "/home/me/project/src",
  kind: "directory" as const,
  size_bytes: 0,
  modified_at_ms: null,
};
const fileEntry = {
  name: "alpha.txt",
  path: "/home/me/project/alpha.txt",
  kind: "file" as const,
  size_bytes: 12,
  modified_at_ms: null,
};
const symlinkEntry = {
  name: "latest.log",
  path: "/home/me/project/latest.log",
  kind: "symlink" as const,
  size_bytes: 24,
  modified_at_ms: null,
};

function panelProps() {
  return {
    attachedSessionId: sessionId,
    activeTab: "files" as const,
    files: {
      session_id: sessionId,
      path: "/home/me/project",
      entries: [directoryEntry, fileEntry, symlinkEntry],
    },
    loading: false,
    gitLoading: false,
    followTerminalCwd: true,
    onTabChange: vi.fn(),
    onOpenDirectory: vi.fn(),
    onOpenFile: vi.fn(),
    onOpenGitFile: vi.fn(),
    onOpenGitDiff: vi.fn(),
    onGitAction: vi.fn(),
    onGoToPath: vi.fn(),
    onRefresh: vi.fn(),
    onRefreshGit: vi.fn(),
    onDismissError: vi.fn(),
    onDismissGitError: vi.fn(),
    onFollowTerminalCwdChange: vi.fn(),
    onUpload: vi.fn(),
    onDownload: vi.fn(),
    onDelete: vi.fn(),
    onHide: vi.fn(),
  };
}

function fileRow(name: string): HTMLDivElement {
  const row = screen.getByText(name).closest<HTMLDivElement>(".file-row");
  if (!row) {
    throw new Error(`Could not find file row for ${name}`);
  }
  return row;
}

function ModalFilesPanel({
  props,
  onClose,
}: {
  props: ReturnType<typeof panelProps>;
  onClose: () => void;
}) {
  const dialogRef = useModalFocus<HTMLDivElement>({
    open: true,
    onClose,
    restoreFocus: false,
  });
  return (
    <div ref={dialogRef} role="dialog" aria-label="Files modal">
      <SessionFilesPanel {...props} />
    </div>
  );
}

describe("SessionFilesPanel file rows", () => {
  it("hides directory sizes and opens folders and files with a double click", async () => {
    const user = userEvent.setup();
    const props = panelProps();
    render(<SessionFilesPanel {...props} />);

    const directoryRow = fileRow("src");
    const regularFileRow = fileRow("alpha.txt");
    expect(within(directoryRow).queryByText("0 B")).not.toBeInTheDocument();
    expect(within(regularFileRow).getByText("12 B")).toHaveClass("file-size");

    await user.dblClick(directoryRow);
    expect(props.onOpenDirectory).toHaveBeenCalledWith(directoryEntry.path);
    await user.dblClick(regularFileRow);
    expect(props.onOpenFile).toHaveBeenCalledWith(fileEntry);
  });

  it("opens a keyboard-accessible right-click menu and closes it predictably", async () => {
    const user = userEvent.setup();
    const props = panelProps();
    render(<SessionFilesPanel {...props} />);

    const row = fileRow("alpha.txt");
    fireEvent.contextMenu(row, { clientX: 140, clientY: 96 });

    const menu = await screen.findByRole("menu", { name: "Actions for alpha.txt" });
    const edit = within(menu).getByRole("menuitem", { name: "Edit alpha.txt" });
    const download = within(menu).getByRole("menuitem", { name: "Download alpha.txt" });
    expect(edit).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(download).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu", { name: "Actions for alpha.txt" })).not.toBeInTheDocument();
    expect(row).toHaveFocus();

    fireEvent.contextMenu(row, { clientX: 140, clientY: 96 });
    await user.click(await screen.findByRole("menuitem", { name: "Download alpha.txt" }));
    expect(props.onDownload).toHaveBeenCalledWith(fileEntry);
    expect(screen.queryByRole("menu", { name: "Actions for alpha.txt" })).not.toBeInTheDocument();
  });

  it("does not offer editing for non-regular file entries", async () => {
    const props = panelProps();
    render(<SessionFilesPanel {...props} />);

    fireEvent.contextMenu(fileRow("latest.log"), { clientX: 140, clientY: 96 });
    const menu = await screen.findByRole("menu", { name: "Actions for latest.log" });
    expect(within(menu).queryByRole("menuitem", { name: "Edit latest.log" })).not.toBeInTheDocument();
    expect(within(menu).getByRole("menuitem", { name: "Download latest.log" })).toBeInTheDocument();
  });

  it("opens the same folder actions through the Context Menu keyboard key", async () => {
    const user = userEvent.setup();
    const props = panelProps();
    render(<SessionFilesPanel {...props} />);

    const row = fileRow("src");
    row.focus();
    await user.keyboard("{ContextMenu}");

    const menu = await screen.findByRole("menu", { name: "Actions for src" });
    expect(within(menu).getByRole("menuitem", { name: "Open src" })).toBeInTheDocument();
    expect(within(menu).queryByRole("menuitem", { name: "Download src" })).not.toBeInTheDocument();
    await user.click(within(menu).getByRole("menuitem", { name: "Delete src" }));
    expect(props.onDelete).toHaveBeenCalledWith(directoryEntry);
  });

  it("keeps the action menu inside a modal focus boundary", async () => {
    const user = userEvent.setup();
    const props = panelProps();
    const onClose = vi.fn();
    render(<ModalFilesPanel props={props} onClose={onClose} />);

    const row = fileRow("alpha.txt");
    await user.click(within(row).getByRole("button", { name: "Actions for alpha.txt" }));
    const menu = await screen.findByRole("menu", { name: "Actions for alpha.txt" });
    await waitFor(() => expect(menu).toBeVisible());
    expect(within(menu).getByRole("menuitem", { name: "Download alpha.txt" })).toBeInTheDocument();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu", { name: "Actions for alpha.txt" })).not.toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Files modal" })).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
    expect(row).toHaveFocus();
  });
});
