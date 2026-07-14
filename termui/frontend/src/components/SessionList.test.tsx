import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CollapsedSessionButton, SessionList } from "./SessionList";

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

  it("重命名保存时把当前输入框里的完整值交给回调", async () => {
    const user = userEvent.setup();
    const onRenameDraftChange = vi.fn();
    const onSaveRename = vi.fn();

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
        renamingSessionId="00000000-0000-0000-0000-000000000401"
        renameDraft="work shell"
        canSaveRename
        onAttach={vi.fn()}
        onStartRename={vi.fn()}
        onRenameDraftChange={onRenameDraftChange}
        onSaveRename={onSaveRename}
        onCancelRename={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Save session name" }));

    expect(onSaveRename).toHaveBeenCalledWith(
      "00000000-0000-0000-0000-000000000401",
      "work shell",
    );
  });

  it("在展开列表保留选中和新输出样式并展示 AI 状态", () => {
    render(
      <SessionList
        sessions={[
          {
            session_id: "00000000-0000-0000-0000-000000000401",
            name: "active",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            activity: { kind: "ai", agent: "codex", state: "running", changed_at_ms: 10 },
          },
          {
            session_id: "00000000-0000-0000-0000-000000000402",
            name: "waiting",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            activity: { kind: "ai", agent: "claude_code", state: "attention", changed_at_ms: 20 },
          },
          {
            session_id: "00000000-0000-0000-0000-000000000403",
            name: "done",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            activity: { kind: "ai", agent: "opencode", state: "completed", changed_at_ms: 30 },
          },
          {
            session_id: "00000000-0000-0000-0000-000000000404",
            name: "ready",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            activity: { kind: "ai", agent: "zcode", state: "idle", changed_at_ms: 40 },
          },
        ]}
        selectedSessionId="00000000-0000-0000-0000-000000000401"
        newOutputSessionIds={new Set(["00000000-0000-0000-0000-000000000401"])}
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

    const runningButton = screen.getByRole("button", {
      name: "Open active, new output, Codex is running",
    });
    expect(screen.getByRole("button", {
      name: "Open waiting, Claude Code needs attention",
    })).toBeInTheDocument();
    expect(screen.getByRole("button", {
      name: "Open done, OpenCode finished",
    })).toBeInTheDocument();
    expect(screen.getByRole("button", {
      name: "Open ready, ZCode is ready",
    })).toBeInTheDocument();
    expect(runningButton.closest(".session-row"))
      .toHaveClass("selected", "has-new-output", "activity-running");
    expect(within(runningButton).getByTitle("Codex is running")).toHaveAttribute("aria-hidden", "true");
  });

  it("在折叠 rail 展示完成状态并保留新输出标记", () => {
    render(
      <CollapsedSessionButton
        session={{
          session_id: "00000000-0000-0000-0000-000000000403",
          name: "done",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          activity: { kind: "ai", agent: "zcode", state: "completed", changed_at_ms: 30 },
        }}
        selected
        hasNewOutput
        onAttach={vi.fn()}
      />,
    );

    const button = screen.getByRole("button", {
      name: "Select done, new output, ZCode finished",
    });
    expect(button).toHaveClass("selected-session-dot", "has-new-output", "activity-completed");
    expect(within(button).getByTitle("ZCode finished")).toHaveClass("compact");
    expect(within(button).getByTitle("ZCode finished")).toHaveAttribute("aria-hidden", "true");
  });
});
