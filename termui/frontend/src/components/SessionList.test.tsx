import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CollapsedSessionButton, SessionList } from "./SessionList";

function fireCardPointer(
  target: HTMLElement,
  type: "pointerdown" | "pointermove" | "pointerup",
  options: { pointerId: number; clientY: number },
): void {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: options.pointerId },
    pointerType: { value: "mouse" },
    button: { value: 0 },
    clientX: { value: 0 },
    clientY: { value: options.clientY },
  });
  fireEvent(target, event);
}

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
    expect(within(row as HTMLElement).queryByRole("button", { name: /Drag/ })).toBeNull();
    const identicon = (row as HTMLElement).querySelector<HTMLImageElement>(
      '[data-session-identicon="00000000-0000-0000-0000-000000000401"] img',
    );
    expect(identicon?.getAttribute("src")).toMatch(/^data:image\/svg\+xml;charset=utf-8,/);
    expect(within(row as HTMLElement).getByRole("button", { name: "Rename session" })).toBeInTheDocument();
    expect(within(row as HTMLElement).getByRole("button", { name: "Close session" })).toBeInTheDocument();
  });

  it("同一 session id 生成稳定的本地图标，不同 id 生成不同图标", () => {
    const alphaId = "00000000-0000-0000-0000-000000000401";
    const betaId = "00000000-0000-0000-0000-000000000402";
    const sessionBase = {
      state: "running" as const,
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    };
    render(
      <>
        <SessionList
          sessions={[
            { ...sessionBase, session_id: alphaId, name: "alpha" },
            { ...sessionBase, session_id: betaId, name: "beta" },
          ]}
          renameDraft=""
          canSaveRename={false}
          onAttach={vi.fn()}
          onStartRename={vi.fn()}
          onRenameDraftChange={vi.fn()}
          onSaveRename={vi.fn()}
          onCancelRename={vi.fn()}
          onClose={vi.fn()}
        />
        <div className="collapsed-session-list">
          <CollapsedSessionButton
            session={{ ...sessionBase, session_id: alphaId, name: "alpha" }}
            selected={false}
            hasNewOutput={false}
            onAttach={vi.fn()}
          />
        </div>
      </>,
    );

    const alphaDarkIcons = Array.from(
      document.querySelectorAll<HTMLImageElement>(
        `[data-session-identicon="${alphaId}"] [data-identicon-theme="dark"]`,
      ),
    );
    const alphaLightIcons = Array.from(
      document.querySelectorAll<HTMLImageElement>(
        `[data-session-identicon="${alphaId}"] [data-identicon-theme="light"]`,
      ),
    );
    const betaDarkIcon = document.querySelector<HTMLImageElement>(
      `[data-session-identicon="${betaId}"] [data-identicon-theme="dark"]`,
    );
    const alphaDarkSources = alphaDarkIcons.map((icon) => icon.getAttribute("src"));
    const alphaLightSources = alphaLightIcons.map((icon) => icon.getAttribute("src"));

    expect(alphaDarkIcons).toHaveLength(2);
    expect(alphaLightIcons).toHaveLength(2);
    expect(alphaDarkSources[0]).toBe(alphaDarkSources[1]);
    expect(alphaLightSources[0]).toBe(alphaLightSources[1]);
    expect(alphaDarkSources[0]).toMatch(/^data:image\/svg\+xml;charset=utf-8,/);
    expect(alphaLightSources[0]).toMatch(/^data:image\/svg\+xml;charset=utf-8,/);
    expect(alphaLightSources[0]).not.toBe(alphaDarkSources[0]);
    expect(betaDarkIcon?.getAttribute("src")).toMatch(/^data:image\/svg\+xml;charset=utf-8,/);
    expect(betaDarkIcon?.getAttribute("src")).not.toBe(alphaDarkSources[0]);
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
    const runningRow = runningButton.closest(".session-row") as HTMLElement;
    const runningIndicator = within(runningRow).getByTitle("Codex is running");
    expect(runningIndicator).toHaveAttribute("aria-hidden", "true");
    expect(runningIndicator.querySelector(".session-activity-work-gear")).not.toBeNull();
    expect(within(runningButton).queryByTitle("Codex is running")).toBeNull();
    expect(screen.getByTitle("OpenCode finished").querySelector(".session-activity-ok-badge")).not.toBeNull();
    expect(screen.getByTitle("Claude Code needs attention").querySelector(".session-activity-attention-badge")).not.toBeNull();
    expect(document.querySelector(".session-identicon")).toBeNull();
  });

  it("直接拖动卡片时用横线标出插入位置并按该位置排序", () => {
    const onAttach = vi.fn();
    const onReorder = vi.fn();
    render(
      <SessionList
        sessions={[
          {
            session_id: "00000000-0000-0000-0000-000000000401",
            name: "alpha",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
          {
            session_id: "00000000-0000-0000-0000-000000000402",
            name: "beta",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
          {
            session_id: "00000000-0000-0000-0000-000000000403",
            name: "gamma",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
        ]}
        renameDraft=""
        canSaveRename={false}
        onAttach={onAttach}
        onStartRename={vi.fn()}
        onRenameDraftChange={vi.fn()}
        onSaveRename={vi.fn()}
        onCancelRename={vi.fn()}
        onClose={vi.fn()}
        onReorder={onReorder}
      />,
    );

    const rows = Array.from(document.querySelectorAll<HTMLElement>(".session-row"));
    rows.forEach((row, index) => {
      row.getBoundingClientRect = vi.fn(() => ({
        x: 0,
        y: index * 60,
        width: 260,
        height: 52,
        top: index * 60,
        right: 260,
        bottom: index * 60 + 52,
        left: 0,
        toJSON: () => ({}),
      }));
    });
    rows[0].setPointerCapture = vi.fn();

    fireCardPointer(rows[0], "pointerdown", { pointerId: 7, clientY: 20 });
    fireCardPointer(rows[0], "pointermove", { pointerId: 7, clientY: 116 });

    expect(rows[0].setPointerCapture).toHaveBeenCalledWith(7);
    expect(rows[2]).toHaveClass("drop-before");
    expect(document.querySelector(".session-activity-slot")).not.toBeNull();

    fireCardPointer(rows[0], "pointerup", { pointerId: 7, clientY: 116 });

    expect(onReorder).toHaveBeenCalledWith([
      "00000000-0000-0000-0000-000000000402",
      "00000000-0000-0000-0000-000000000401",
      "00000000-0000-0000-0000-000000000403",
    ]);
    expect(onAttach).not.toHaveBeenCalled();
  });

  it("短按会话卡片时保留打开按钮的点击目标", () => {
    const onAttach = vi.fn();
    render(
      <SessionList
        sessions={[
          {
            session_id: "00000000-0000-0000-0000-000000000401",
            name: "alpha",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
        ]}
        renameDraft=""
        canSaveRename={false}
        onAttach={onAttach}
        onStartRename={vi.fn()}
        onRenameDraftChange={vi.fn()}
        onSaveRename={vi.fn()}
        onCancelRename={vi.fn()}
        onClose={vi.fn()}
        onReorder={vi.fn()}
      />,
    );

    const openButton = screen.getByRole("button", { name: "Open alpha" });
    const row = openButton.closest(".session-row") as HTMLElement;
    let captured = false;
    row.setPointerCapture = vi.fn(() => {
      captured = true;
    });

    fireCardPointer(openButton, "pointerdown", { pointerId: 9, clientY: 20 });
    const clickTarget = captured ? row : openButton;
    fireCardPointer(clickTarget, "pointerup", { pointerId: 9, clientY: 20 });
    fireEvent.click(clickTarget);

    expect(onAttach).toHaveBeenCalledOnce();
    expect(onAttach).toHaveBeenCalledWith("00000000-0000-0000-0000-000000000401");
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
