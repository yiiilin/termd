import type { UUID } from "./protocol/types";

export const SESSION_OPEN_STEP_IDS = [
  "selected",
  "connecting",
  "attaching",
  "initializing",
  "syncing",
] as const;

export type SessionOpenStepId = (typeof SESSION_OPEN_STEP_IDS)[number];
export type SessionOpenProgressStatus = "opening" | "ready" | "failed";

export interface SessionOpenProgressStep {
  id: SessionOpenStepId;
  startedAtMs?: number;
  endedAtMs?: number;
}

export interface SessionOpenProgress {
  attemptId: number;
  sessionId: UUID;
  sessionName: string;
  status: SessionOpenProgressStatus;
  startedAtMs: number;
  finishedAtMs?: number;
  failedStepId?: SessionOpenStepId;
  steps: SessionOpenProgressStep[];
}

interface StartSessionOpenProgressOptions {
  attemptId: number;
  sessionId: UUID;
  sessionName: string;
  nowMs?: number;
}

export function startSessionOpenProgress(
  options: StartSessionOpenProgressOptions,
): SessionOpenProgress {
  const nowMs = options.nowMs ?? Date.now();
  return {
    attemptId: options.attemptId,
    sessionId: options.sessionId,
    sessionName: options.sessionName,
    status: "opening",
    startedAtMs: nowMs,
    steps: SESSION_OPEN_STEP_IDS.map((id, index) => ({
      id,
      startedAtMs: index <= 1 ? nowMs : undefined,
      endedAtMs: index === 0 ? nowMs : undefined,
    })),
  };
}

export function advanceSessionOpenProgress(
  progress: SessionOpenProgress | undefined,
  attemptId: number,
  stepId: SessionOpenStepId,
  nowMs = Date.now(),
): SessionOpenProgress | undefined {
  if (!progress || progress.attemptId !== attemptId || progress.status !== "opening") {
    return progress;
  }
  const targetIndex = SESSION_OPEN_STEP_IDS.indexOf(stepId);
  const currentIndex = progress.steps.reduce(
    (latest, step, index) => step.startedAtMs === undefined ? latest : index,
    -1,
  );
  if (targetIndex <= currentIndex) {
    return progress;
  }
  const transitionAtMs = Math.max(nowMs, progress.startedAtMs);
  return {
    ...progress,
    steps: progress.steps.map((step, index) => {
      if (index < targetIndex && step.endedAtMs === undefined) {
        return {
          ...step,
          startedAtMs: step.startedAtMs ?? transitionAtMs,
          endedAtMs: transitionAtMs,
        };
      }
      if (index === targetIndex && step.startedAtMs === undefined) {
        return { ...step, startedAtMs: transitionAtMs };
      }
      return step;
    }),
  };
}

export function completeSessionOpenProgress(
  progress: SessionOpenProgress | undefined,
  attemptId: number,
  nowMs = Date.now(),
): SessionOpenProgress | undefined {
  const syncing = advanceSessionOpenProgress(progress, attemptId, "syncing", nowMs);
  if (!syncing || syncing.attemptId !== attemptId || syncing.status !== "opening") {
    return syncing;
  }
  const finishedAtMs = Math.max(nowMs, syncing.startedAtMs);
  return {
    ...syncing,
    status: "ready",
    finishedAtMs,
    steps: syncing.steps.map((step) => ({
      ...step,
      startedAtMs: step.startedAtMs ?? finishedAtMs,
      endedAtMs: step.endedAtMs ?? finishedAtMs,
    })),
  };
}

export function failSessionOpenProgress(
  progress: SessionOpenProgress | undefined,
  attemptId: number,
  nowMs = Date.now(),
): SessionOpenProgress | undefined {
  if (!progress || progress.attemptId !== attemptId || progress.status !== "opening") {
    return progress;
  }
  const failedStepIndex = progress.steps.reduce(
    (latest, step, index) => step.startedAtMs === undefined ? latest : index,
    0,
  );
  const failedStep = progress.steps[failedStepIndex];
  const finishedAtMs = Math.max(nowMs, progress.startedAtMs);
  return {
    ...progress,
    status: "failed",
    finishedAtMs,
    failedStepId: failedStep.id,
    steps: progress.steps.map((step, index) => (
      index === failedStepIndex && step.endedAtMs === undefined
        ? { ...step, endedAtMs: finishedAtMs }
        : step
    )),
  };
}

export function sessionOpenProgressCurrentStep(
  progress: SessionOpenProgress,
): SessionOpenProgressStep | undefined {
  if (progress.status !== "opening") {
    return undefined;
  }
  return progress.steps.find((step) => step.startedAtMs !== undefined && step.endedAtMs === undefined);
}
