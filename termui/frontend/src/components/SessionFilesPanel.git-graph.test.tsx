import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { SessionGitFileChangePayload, SessionGitResultPayload, UUID } from "../protocol/types";
import { SessionFilesPanel } from "./SessionFilesPanel";

const SESSION_ID = "00000000-0000-0000-0000-000000000701" as UUID;

function renderGitGraph(graph: string[], unstaged: SessionGitFileChangePayload[] = []) {
  const onOpenGitDiff = vi.fn();
  const git: SessionGitResultPayload = {
    session_id: SESSION_ID,
    cwd: "/home/me/termd",
    repository_root: "/home/me/termd",
    worktrees: [
      {
        path: "/home/me/termd",
        branch: "main",
        head: "a1b2c3d",
        is_current: true,
        staged: [],
        unstaged,
      },
    ],
    graph,
    error: null,
  };

  render(
    <SessionFilesPanel
      attachedSessionId={SESSION_ID}
      activeTab="git"
      loading={false}
      git={git}
      gitLoading={false}
      followTerminalCwd
      onTabChange={vi.fn()}
      onOpenDirectory={vi.fn()}
      onOpenFile={vi.fn()}
      onOpenGitFile={vi.fn()}
      onOpenGitDiff={onOpenGitDiff}
      onGitAction={vi.fn()}
      onGoToPath={vi.fn()}
      onRefresh={vi.fn()}
      onRefreshGit={vi.fn()}
      onDismissError={vi.fn()}
      onDismissGitError={vi.fn()}
      onFollowTerminalCwdChange={vi.fn()}
      onUpload={vi.fn()}
      onDownload={vi.fn()}
      onDelete={vi.fn()}
      onHide={vi.fn()}
    />,
  );

  return { git, onOpenGitDiff };
}

describe("SessionFilesPanel Git graph", () => {
  it("区分 HEAD、本地分支、远端分支和 tag", () => {
    renderGitGraph([
      "* a1b2c3d (HEAD -> main, origin/main) feat: improve graph",
      "| * d4e5f6a (feature/quick-keys, tag: v0.8.13) fix: mobile input",
      "|/",
    ]);

    const graph = screen.getByLabelText("Git graph commits");
    const headRow = graph.querySelector<HTMLElement>(".git-graph-row-head");
    expect(headRow).not.toBeNull();
    expect(within(headRow!).getByText("HEAD")).toHaveClass("git-graph-ref-head");
    expect(within(headRow!).getByText("main")).toHaveClass("git-graph-ref-branch");
    expect(within(headRow!).getByText("origin/main")).toHaveClass("git-graph-ref-remote");
    expect(within(graph).getByText("v0.8.13")).toHaveClass("git-graph-ref-tag");
    expect(graph.querySelectorAll(".git-graph-row-connector")).toHaveLength(1);
  });

  it("点击变更文件行会打开 diff", () => {
    const change = { path: "src/main.rs", status: " M" };
    const { git, onOpenGitDiff } = renderGitGraph([], [change]);
    const changeRow = screen.getByRole("treeitem", { name: "M src/main.rs" });

    fireEvent.click(changeRow);

    expect(onOpenGitDiff).toHaveBeenCalledWith(git.worktrees[0], change, false);

    onOpenGitDiff.mockClear();
    fireEvent.click(within(changeRow).getByRole("button", { name: "Diff src/main.rs" }));
    expect(onOpenGitDiff).toHaveBeenCalledTimes(1);
  });
});
