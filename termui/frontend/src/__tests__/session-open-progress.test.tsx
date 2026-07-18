import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionOpenProgressControl, formatSessionOpenDuration } from "../components/SessionOpenProgressControl";
import {
  advanceSessionOpenProgress,
  completeSessionOpenProgress,
  failSessionOpenProgress,
  startSessionOpenProgress,
} from "../session-open-progress";

const SESSION_ID = "00000000-0000-0000-0000-000000000901";

afterEach(() => {
  vi.useRealTimers();
});

describe("session open progress state", () => {
  it("records real stage boundaries and ignores stale attempts", () => {
    const started = startSessionOpenProgress({
      attemptId: 7,
      sessionId: SESSION_ID,
      sessionName: "alpha",
      nowMs: 1_000,
    });

    expect(started.steps).toEqual([
      { id: "selected", startedAtMs: 1_000, endedAtMs: 1_000 },
      { id: "connecting", startedAtMs: 1_000, endedAtMs: undefined },
      { id: "attaching", startedAtMs: undefined, endedAtMs: undefined },
      { id: "initializing", startedAtMs: undefined, endedAtMs: undefined },
      { id: "syncing", startedAtMs: undefined, endedAtMs: undefined },
    ]);

    expect(advanceSessionOpenProgress(started, 6, "attaching", 1_300)).toBe(started);
    const attaching = advanceSessionOpenProgress(started, 7, "attaching", 1_300)!;
    expect(attaching.steps[1]).toEqual({
      id: "connecting",
      startedAtMs: 1_000,
      endedAtMs: 1_300,
    });
    expect(attaching.steps[2]).toEqual({
      id: "attaching",
      startedAtMs: 1_300,
      endedAtMs: undefined,
    });

    const initializing = advanceSessionOpenProgress(attaching, 7, "initializing", 1_725)!;
    const syncing = advanceSessionOpenProgress(initializing, 7, "syncing", 1_900)!;
    const ready = completeSessionOpenProgress(syncing, 7, 2_150)!;

    expect(ready.status).toBe("ready");
    expect(ready.finishedAtMs).toBe(2_150);
    expect(ready.steps.map((step) => step.endedAtMs)).toEqual([
      1_000,
      1_300,
      1_725,
      1_900,
      2_150,
    ]);
  });

  it("stops a failed attempt at its current stage", () => {
    const started = startSessionOpenProgress({
      attemptId: 4,
      sessionId: SESSION_ID,
      sessionName: "alpha",
      nowMs: 500,
    });
    const failed = failSessionOpenProgress(started, 4, 900)!;

    expect(failed.status).toBe("failed");
    expect(failed.failedStepId).toBe("connecting");
    expect(failed.steps[1].endedAtMs).toBe(900);
    expect(failed.steps[2].startedAtMs).toBeUndefined();
  });
});

describe("SessionOpenProgressControl", () => {
  it("opens during attach, shows the next step, then collapses and remains available for review", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(10_000));
    const opening = startSessionOpenProgress({
      attemptId: 11,
      sessionId: SESSION_ID,
      sessionName: "alpha",
      nowMs: 10_000,
    });
    const { rerender } = render(<SessionOpenProgressControl progress={opening} />);

    const button = screen.getByRole("button", { name: "Opening progress for alpha" });
    expect(button).toHaveAttribute("aria-expanded", "true");
    expect(button.querySelector("svg[data-open-progress-icon='opening']"))
      .toHaveClass("terminal-open-progress-icon", "lucide-loader-circle");
    expect(button.querySelector("img[data-session-avatar]")).toBeNull();
    const openingProgress = screen.getByTestId("terminal-open-progress");
    expect(openingProgress).toHaveTextContent("Opening terminal");
    expect(openingProgress).toHaveTextContent("Connect terminal channel");
    expect(openingProgress).toHaveTextContent("NextAttach session");
    expect(openingProgress).not.toHaveAttribute("aria-live");
    expect(openingProgress.querySelector("footer")).toHaveAttribute("aria-live", "polite");

    act(() => {
      vi.advanceTimersByTime(350);
    });
    expect(screen.getByTestId("terminal-open-progress")).toHaveTextContent("300 ms");

    const attaching = advanceSessionOpenProgress(opening, 11, "attaching", 10_350)!;
    const initializing = advanceSessionOpenProgress(attaching, 11, "initializing", 10_600)!;
    const syncing = advanceSessionOpenProgress(initializing, 11, "syncing", 10_750)!;
    const ready = completeSessionOpenProgress(syncing, 11, 11_000)!;
    rerender(<SessionOpenProgressControl progress={ready} />);

    expect(screen.queryByTestId("terminal-open-progress")).toBeNull();
    const readyButton = screen.getByRole("button", { name: "Session opening progress" });
    expect(readyButton).toHaveAttribute("aria-expanded", "false");
    expect(readyButton.querySelector("svg[data-open-progress-icon='ready']"))
      .toHaveClass("terminal-open-progress-icon", "lucide-list-checks");

    fireEvent.click(readyButton);
    expect(screen.getByTestId("terminal-open-progress")).toHaveTextContent("Terminal ready");
    expect(screen.getByTestId("terminal-open-progress")).toHaveTextContent("Sync terminal content");
    expect(screen.getByTestId("terminal-open-progress")).toHaveTextContent("NextTerminal available");
    expect(screen.queryByRole("button", { name: "Search terminal" })).toBeNull();
  });

  it("formats sub-second, second, and minute durations without layout-dependent units", () => {
    expect(formatSessionOpenDuration(245)).toBe("245 ms");
    expect(formatSessionOpenDuration(2_450)).toBe("2.5 s");
    expect(formatSessionOpenDuration(62_900)).toBe("1m 02s");
  });
});
