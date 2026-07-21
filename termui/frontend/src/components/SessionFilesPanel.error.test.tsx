import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SessionFilesPanel } from "./SessionFilesPanel";

const sessionId = "00000000-0000-0000-0000-0000000004e1";

function panelProps() {
  return {
    attachedSessionId: sessionId,
    activeTab: "files" as const,
    files: {
      session_id: sessionId,
      path: "/home/me/project",
      entries: [
        { name: "alpha.txt", path: "/home/me/project/alpha.txt", kind: "file" as const, size_bytes: 12, modified_at_ms: null },
      ],
    },
    loading: false,
    error: { code: "connection_error", message: "Authorization Bearer raw-secret" },
    git: {
      session_id: sessionId,
      cwd: "/home/me/project",
      repository_root: "/home/me/project",
      worktrees: [{
        path: "/home/me/project",
        branch: "main",
        head: "a1b2c3d",
        is_current: true,
        staged: [],
        unstaged: [{ path: "README.md", status: " M" }],
      }],
      graph: ["* a1b2c3d cached commit"],
      error: null,
    },
    gitLoading: false,
    gitError: { code: "connection_error", message: "token=raw-secret" },
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

describe("SessionFilesPanel local errors", () => {
  it("keeps cached files visible and exposes retry, dismiss, and loading state", async () => {
    const user = userEvent.setup();
    const props = panelProps();
    const { rerender } = render(<SessionFilesPanel {...props} />);

    const filesTab = screen.getByRole("tabpanel", { name: "Files" });
    const alert = within(filesTab).getByRole("alert", { name: "Files error" });
    expect(alert).toHaveTextContent("connection error");
    expect(alert).not.toHaveTextContent("raw-secret");
    expect(within(filesTab).getByText("alpha.txt")).toBeInTheDocument();

    await user.click(within(alert).getByRole("button", { name: "Retry" }));
    expect(props.onRefresh).toHaveBeenCalledTimes(1);
    await user.click(within(alert).getByRole("button", { name: "Dismiss files error" }));
    expect(props.onDismissError).toHaveBeenCalledTimes(1);

    rerender(<SessionFilesPanel {...props} loading />);
    const busyFilesTab = screen.getByRole("tabpanel", { name: "Files" });
    expect(busyFilesTab).toHaveAttribute("aria-busy", "true");
    expect(within(busyFilesTab).getByRole("button", { name: "Retrying" })).toBeDisabled();
    expect(within(busyFilesTab).getByText("alpha.txt")).toBeInTheDocument();
  });

  it("keeps cached Git content visible and scopes retry and dismiss to Git", async () => {
    const user = userEvent.setup();
    const props = { ...panelProps(), activeTab: "git" as const };
    render(<SessionFilesPanel {...props} />);

    const gitTab = screen.getByRole("tabpanel", { name: "Git" });
    const alert = within(gitTab).getByRole("alert", { name: "Git error" });
    expect(alert).toHaveTextContent("connection error");
    expect(alert).not.toHaveTextContent("raw-secret");
    expect(within(gitTab).getByText("README.md")).toBeInTheDocument();
    expect(within(gitTab).getByTitle("a1b2c3d cached commit")).toBeInTheDocument();

    await user.click(within(alert).getByRole("button", { name: "Retry" }));
    expect(props.onRefreshGit).toHaveBeenCalledTimes(1);
    expect(props.onRefresh).not.toHaveBeenCalled();
    await user.click(within(alert).getByRole("button", { name: "Dismiss Git error" }));
    expect(props.onDismissGitError).toHaveBeenCalledTimes(1);
    expect(props.onDismissError).not.toHaveBeenCalled();
  });

  it.each([
    { tab: "files" as const, alertName: "Files error" },
    { tab: "git" as const, alertName: "Git error" },
  ])("shows unavailable alongside the $tab error when there is no cache", ({ tab, alertName }) => {
    const props = panelProps();
    render(
      <SessionFilesPanel
        {...props}
        activeTab={tab}
        files={undefined}
        git={undefined}
      />,
    );

    const tabPanel = screen.getByRole("tabpanel", { name: tab === "files" ? "Files" : "Git" });
    expect(within(tabPanel).getByRole("alert", { name: alertName })).toBeInTheDocument();
    expect(within(tabPanel).getAllByText("unavailable").length).toBeGreaterThan(0);
  });

  it("does not render a raw Git payload error", () => {
    const props = panelProps();
    render(
      <SessionFilesPanel
        {...props}
        activeTab="git"
        git={{ ...props.git, error: "Authorization Bearer raw-secret" }}
        gitError={undefined}
      />,
    );

    const gitTab = screen.getByRole("tabpanel", { name: "Git" });
    expect(within(gitTab).getAllByText("unavailable").length).toBeGreaterThan(0);
    expect(gitTab).not.toHaveTextContent("raw-secret");
  });

  it("does not render an unknown transport error message", () => {
    const props = panelProps();
    render(
      <SessionFilesPanel
        {...props}
        error={{ code: "upstream_failure", message: "Authorization Bearer raw-secret" }}
      />,
    );

    const alert = screen.getByRole("alert", { name: "Files error" });
    expect(alert).toHaveTextContent("protocol operation failed");
    expect(alert).not.toHaveTextContent("raw-secret");
  });

  it("keeps a known translated error even when its source message matches the translation", () => {
    const props = panelProps();
    render(
      <SessionFilesPanel
        {...props}
        error={{ code: "connection_error", message: "connection error" }}
      />,
    );

    expect(screen.getByRole("alert", { name: "Files error" })).toHaveTextContent("connection error");
  });
});
