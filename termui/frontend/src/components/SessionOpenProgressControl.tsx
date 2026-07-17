import { useEffect, useMemo, useState } from "react";
import { Check, Circle, CircleX, ListChecks, LoaderCircle } from "lucide-react";
import { useI18n, type Translate } from "../i18n";
import {
  sessionOpenProgressCurrentStep,
  type SessionOpenProgress,
  type SessionOpenProgressStep,
  type SessionOpenStepId,
} from "../session-open-progress";

interface SessionOpenProgressControlProps {
  progress: SessionOpenProgress;
}

export function SessionOpenProgressControl({ progress }: SessionOpenProgressControlProps) {
  const { t } = useI18n();
  const [detailsOpen, setDetailsOpen] = useState(progress.status === "opening");
  const [nowMs, setNowMs] = useState(() => progress.finishedAtMs ?? Date.now());

  useEffect(() => {
    setDetailsOpen(progress.status === "opening");
  }, [progress.attemptId]);

  useEffect(() => {
    if (progress.status === "ready") {
      setDetailsOpen(false);
    } else if (progress.status === "failed") {
      setDetailsOpen(true);
    }
  }, [progress.status]);

  useEffect(() => {
    if (progress.status !== "opening") {
      setNowMs(progress.finishedAtMs ?? Date.now());
      return undefined;
    }
    const updateNow = () => setNowMs(Date.now());
    updateNow();
    const timer = window.setInterval(updateNow, 100);
    return () => window.clearInterval(timer);
  }, [progress.attemptId, progress.finishedAtMs, progress.status]);

  const currentStep = sessionOpenProgressCurrentStep(progress);
  const currentStepIndex = currentStep
    ? progress.steps.findIndex((step) => step.id === currentStep.id)
    : -1;
  const nextStep = progress.status === "opening"
    ? progress.steps[currentStepIndex + 1]
    : undefined;
  const totalDuration = formatSessionOpenDuration(
    (progress.finishedAtMs ?? nowMs) - progress.startedAtMs,
  );
  const buttonLabel = progress.status === "opening"
    ? t("terminal.openProgress.openingAria", { name: progress.sessionName })
    : t("terminal.openProgress.aria");
  const title = progress.status === "opening"
    ? t("terminal.openProgress.opening", { name: progress.sessionName })
    : progress.status === "ready"
      ? t("terminal.openProgress.ready")
      : t("terminal.openProgress.failed");
  const nextLabel = progress.status === "ready"
    ? t("terminal.openProgress.available")
    : progress.status === "failed"
      ? t("terminal.openProgress.retry")
      : nextStep
        ? sessionOpenStepLabel(nextStep.id, t)
        : t("terminal.openProgress.available");

  const stepStates = useMemo(
    () => progress.steps.map((step) => ({
      step,
      state: sessionOpenStepState(progress, step),
    })),
    [progress],
  );
  return (
    <div className="terminal-open-progress-control" onClick={(event) => event.stopPropagation()}>
      <button
        type="button"
        className={`icon-button terminal-open-progress-button ${progress.status}`}
        aria-label={buttonLabel}
        title={buttonLabel}
        aria-expanded={detailsOpen}
        aria-controls="terminal-open-progress-popover"
        onClick={() => setDetailsOpen((open) => !open)}
      >
        {progress.status === "opening" ? (
          <LoaderCircle
            className="terminal-open-progress-icon"
            data-open-progress-icon="opening"
            size={18}
            aria-hidden="true"
          />
        ) : progress.status === "ready" ? (
          <ListChecks
            className="terminal-open-progress-icon"
            data-open-progress-icon="ready"
            size={18}
            aria-hidden="true"
          />
        ) : (
          <CircleX
            className="terminal-open-progress-icon"
            data-open-progress-icon="failed"
            size={18}
            aria-hidden="true"
          />
        )}
      </button>
      {detailsOpen ? (
        <section
          className={`terminal-open-progress-popover ${progress.status}`}
          id="terminal-open-progress-popover"
          data-testid="terminal-open-progress"
        >
          <header className="terminal-open-progress-header">
            <div>
              <strong>{title}</strong>
              <span>{progress.sessionName}</span>
            </div>
            <time>{totalDuration}</time>
          </header>
          <ol className="terminal-open-progress-steps">
            {stepStates.map(({ step, state }) => (
              <li key={step.id} className={state}>
                <span className="terminal-open-progress-step-icon" aria-hidden="true">
                  {state === "completed" ? (
                    <Check size={12} />
                  ) : state === "current" ? (
                    <LoaderCircle size={12} />
                  ) : state === "failed" ? (
                    <CircleX size={12} />
                  ) : (
                    <Circle size={10} />
                  )}
                </span>
                <span>{sessionOpenStepLabel(step.id, t)}</span>
                <time>{sessionOpenStepDuration(step, state, nowMs)}</time>
              </li>
            ))}
          </ol>
          <footer className="terminal-open-progress-next" aria-live="polite">
            <span>{t("terminal.openProgress.next")}</span>
            <strong>{nextLabel}</strong>
          </footer>
        </section>
      ) : null}
    </div>
  );
}

type SessionOpenStepState = "completed" | "current" | "pending" | "failed";

function sessionOpenStepState(
  progress: SessionOpenProgress,
  step: SessionOpenProgressStep,
): SessionOpenStepState {
  if (progress.status === "failed" && progress.failedStepId === step.id) {
    return "failed";
  }
  if (step.endedAtMs !== undefined) {
    return "completed";
  }
  if (step.startedAtMs !== undefined) {
    return "current";
  }
  return "pending";
}

function sessionOpenStepLabel(stepId: SessionOpenStepId, t: Translate): string {
  switch (stepId) {
    case "selected":
      return t("terminal.openProgress.step.selected");
    case "connecting":
      return t("terminal.openProgress.step.connecting");
    case "attaching":
      return t("terminal.openProgress.step.attaching");
    case "initializing":
      return t("terminal.openProgress.step.initializing");
    case "syncing":
      return t("terminal.openProgress.step.syncing");
  }
}

function sessionOpenStepDuration(
  step: SessionOpenProgressStep,
  state: SessionOpenStepState,
  nowMs: number,
): string {
  if (step.startedAtMs === undefined || state === "pending") {
    return "--";
  }
  return formatSessionOpenDuration((step.endedAtMs ?? nowMs) - step.startedAtMs);
}

export function formatSessionOpenDuration(durationMs: number): string {
  const safeDurationMs = Math.max(0, durationMs);
  if (safeDurationMs < 1000) {
    return `${Math.round(safeDurationMs)} ms`;
  }
  if (safeDurationMs < 60_000) {
    return `${(safeDurationMs / 1000).toFixed(1)} s`;
  }
  const minutes = Math.floor(safeDurationMs / 60_000);
  const seconds = Math.floor((safeDurationMs % 60_000) / 1000);
  return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
}
