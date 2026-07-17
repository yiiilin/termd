import { Bot, Check, CircleAlert, Cog, Pencil, Trash2, X } from "lucide-react";
import { useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { flushSync } from "react-dom";
import type { SessionAiActivityPayload, SessionSummaryPayload, UUID } from "../protocol/types";
import { sessionDisplayName } from "../session-names";
import { useI18n, type Translate } from "../i18n";
import { SessionThumbsAvatar } from "./SessionThumbsAvatar";

export function sessionActivityClassName(activity?: SessionAiActivityPayload | null): string {
  return activity ? `activity-${activity.state}` : "";
}

function sessionActivityLabel(t: Translate, activity?: SessionAiActivityPayload | null): string | undefined {
  if (!activity) return undefined;
  const agent = t(`sessions.activity.agent.${activity.agent}`);
  return t(`sessions.activity.${activity.state}`, { agent });
}

function SessionAvatar(props: { sessionId: UUID; compact?: boolean }) {
  return (
    <span
      className={["session-avatar", props.compact ? "compact" : ""].filter(Boolean).join(" ")}
      aria-hidden="true"
    >
      <SessionThumbsAvatar sessionId={props.sessionId} />
    </span>
  );
}

function SessionActivityIndicator(props: {
  activity?: SessionAiActivityPayload | null;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const activity = props.activity;
  if (!activity) return null;
  const label = sessionActivityLabel(t, activity);
  const compact = Boolean(props.compact);
  return (
    <span
      className={["session-activity-indicator", compact ? "compact" : "", sessionActivityClassName(activity)]
        .filter(Boolean)
        .join(" ")}
      aria-hidden="true"
      title={label}
    >
      <Bot className="session-activity-bot" size={compact ? 8 : 10} strokeWidth={2} />
      {activity.state === "running" ? (
        <Cog className="session-activity-work-gear" size={compact ? 6 : 7} strokeWidth={2.2} />
      ) : activity.state === "attention" ? (
        <CircleAlert className="session-activity-attention-badge" size={compact ? 6 : 7} strokeWidth={2.4} />
      ) : (
        <span className="session-activity-ok-badge">
          <Check size={compact ? 5 : 6} strokeWidth={3} />
        </span>
      )}
    </span>
  );
}

function SessionVisual(props: {
  sessionId: UUID;
  activity?: SessionAiActivityPayload | null;
  compact?: boolean;
}) {
  return (
    <span className={["session-visual", props.compact ? "compact" : ""].filter(Boolean).join(" ")}>
      <SessionAvatar sessionId={props.sessionId} compact={props.compact} />
      <SessionActivityIndicator activity={props.activity} compact={props.compact} />
    </span>
  );
}

export function CollapsedSessionButton(props: {
  session: SessionSummaryPayload;
  selected: boolean;
  hasNewOutput: boolean;
  onAttach: (sessionId: UUID) => void;
}) {
  const { t } = useI18n();
  const displayName = sessionDisplayName(props.session);
  const selectLabel = props.hasNewOutput
    ? t("sessions.selectNewOutput", { name: displayName })
    : t("sessions.select", { name: displayName });
  const activityLabel = sessionActivityLabel(t, props.session.activity);
  return (
    <button
      type="button"
      className={[
        "icon-button",
        props.selected ? "selected-session-dot" : "",
        props.hasNewOutput ? "has-new-output" : "",
        sessionActivityClassName(props.session.activity),
      ]
        .filter(Boolean)
        .join(" ")}
      aria-label={activityLabel ? `${selectLabel}, ${activityLabel}` : selectLabel}
      onClick={() => props.onAttach(props.session.session_id)}
    >
      <SessionVisual sessionId={props.session.session_id} activity={props.session.activity} compact />
    </button>
  );
}

interface SessionListProps {
  sessions: SessionSummaryPayload[];
  selectedSessionId?: UUID;
  newOutputSessionIds?: ReadonlySet<UUID>;
  creating?: boolean;
  renamingSessionId?: UUID;
  renameDraft: string;
  canSaveRename: boolean;
  onAttach: (sessionId: UUID) => void;
  onStartRename: (sessionId: UUID, currentName: string) => void;
  onRenameDraftChange: (name: string) => void;
  onSaveRename: (sessionId: UUID, nextName: string) => void;
  onCancelRename: () => void;
  onClose: (sessionId: UUID) => void;
  onReorder?: (sessionIds: UUID[]) => void;
}

const SESSION_DRAG_THRESHOLD_PX = 5;

export function SessionList(props: SessionListProps) {
  const [draggingSessionId, setDraggingSessionId] = useState<UUID | undefined>();
  const [dragInsertionIndex, setDragInsertionIndex] = useState<number | undefined>();
  const { t } = useI18n();
  const rowRefs = useRef(new Map<UUID, HTMLDivElement>());
  const suppressClickUntilRef = useRef(0);
  const pointerDragRef = useRef<{
    pointerId: number;
    sessionId: UUID;
    startY: number;
    dragging: boolean;
  } | null>(null);

  const isActivePointer = (eventPointerId: number | undefined, dragPointerId: number) =>
    eventPointerId === undefined || eventPointerId === 0 || eventPointerId === dragPointerId;

  const moveSessionToIndex = (sessionId: UUID, targetIndex: number) => {
    const currentIds = props.sessions.map((session) => session.session_id);
    const withoutDragging = currentIds.filter((candidateId) => candidateId !== sessionId);
    const clampedIndex = Math.max(0, Math.min(targetIndex, withoutDragging.length));
    const next = [...withoutDragging];
    next.splice(clampedIndex, 0, sessionId);
    if (next.every((candidateId, index) => candidateId === currentIds[index])) {
      return;
    }
    props.onReorder?.(next);
  };

  const moveSessionByOffset = (sessionId: UUID, offset: -1 | 1) => {
    const ids = props.sessions.map((session) => session.session_id);
    const index = ids.indexOf(sessionId);
    const nextIndex = index + offset;
    if (index < 0 || nextIndex < 0 || nextIndex >= ids.length) {
      return;
    }
    const next = [...ids];
    [next[index], next[nextIndex]] = [next[nextIndex], next[index]];
    props.onReorder?.(next);
  };

  const resolveInsertionIndex = (clientY: number, draggedSessionId: UUID) => {
    const rows = props.sessions
      .filter((session) => session.session_id !== draggedSessionId)
      .map((session) => ({
        sessionId: session.session_id,
        rect: rowRefs.current.get(session.session_id)?.getBoundingClientRect(),
      }))
      .filter((entry): entry is { sessionId: UUID; rect: DOMRect } => Boolean(entry.rect));

    for (let index = 0; index < rows.length; index += 1) {
      const row = rows[index];
      if (clientY < row.rect.top + row.rect.height / 2) {
        return index;
      }
    }

    return rows.length;
  };

  const startPointerDrag = (sessionId: UUID, pointerId: number, clientY: number) => {
    pointerDragRef.current = {
      pointerId: pointerId || 1,
      sessionId,
      startY: clientY,
      dragging: false,
    };
  };

  const updatePointerDrag = (clientY: number, pointerId?: number): boolean => {
    let drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return false;
    }
    if (!drag.dragging) {
      if (Math.abs(clientY - drag.startY) < SESSION_DRAG_THRESHOLD_PX) {
        return false;
      }
      drag = { ...drag, dragging: true };
      pointerDragRef.current = drag;
      setDraggingSessionId(drag.sessionId);
    }
    setDragInsertionIndex(resolveInsertionIndex(clientY, drag.sessionId));
    return true;
  };

  const finishPointerDrag = (clientY: number, pointerId?: number): boolean => {
    const drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return false;
    }
    pointerDragRef.current = null;
    if (!drag.dragging) {
      return false;
    }
    const targetIndex = resolveInsertionIndex(clientY, drag.sessionId);
    suppressClickUntilRef.current = Date.now() + 250;
    setDraggingSessionId(undefined);
    setDragInsertionIndex(undefined);
    moveSessionToIndex(drag.sessionId, targetIndex);
    return true;
  };

  const cancelPointerDrag = (pointerId?: number) => {
    const drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return;
    }
    pointerDragRef.current = null;
    setDraggingSessionId(undefined);
    setDragInsertionIndex(undefined);
  };

  const dropCandidates = draggingSessionId
    ? props.sessions.filter((session) => session.session_id !== draggingSessionId)
    : [];
  const dropBeforeSessionId = dragInsertionIndex === undefined
    ? undefined
    : dropCandidates[dragInsertionIndex]?.session_id;
  const dropAfterSessionId = dragInsertionIndex === dropCandidates.length
    ? dropCandidates.at(-1)?.session_id
    : undefined;

  return (
    <section className="session-list" aria-label={t("sessions.aria")}>
      {props.sessions.length === 0 && !props.creating ? <div className="empty-list">{t("sessions.empty")}</div> : null}
      {props.sessions.length === 0 && props.creating ? <div className="empty-list">{t("sessions.creating")}</div> : null}
      {props.sessions.map((session) => {
        const displayName = sessionDisplayName(session);
        const isRenaming = props.renamingSessionId === session.session_id;
        const hasNewOutput = props.newOutputSessionIds?.has(session.session_id) ?? false;
        const openLabel = hasNewOutput
          ? t("sessions.openNewOutput", { name: displayName })
          : t("sessions.open", { name: displayName });
        const activityLabel = sessionActivityLabel(t, session.activity);
        const isReorderable = Boolean(props.onReorder) && !isRenaming;
        const showDropBefore = Boolean(draggingSessionId) && dropBeforeSessionId === session.session_id;
        const showDropAfter = Boolean(draggingSessionId) && dropAfterSessionId === session.session_id;
        return (
          <div
            className={[
              "session-row",
              session.session_id === props.selectedSessionId ? "selected" : "",
              hasNewOutput ? "has-new-output" : "",
              sessionActivityClassName(session.activity),
              isReorderable ? "reorderable" : "",
              draggingSessionId === session.session_id ? "dragging" : "",
              showDropBefore ? "drop-before" : "",
              showDropAfter ? "drop-after" : "",
            ]
              .filter(Boolean)
              .join(" ")}
            key={session.session_id}
            ref={(node) => {
              if (node) {
                rowRefs.current.set(session.session_id, node);
              } else {
                rowRefs.current.delete(session.session_id);
              }
            }}
            onClickCapture={(event) => {
              if (Date.now() >= suppressClickUntilRef.current) {
                return;
              }
              event.preventDefault();
              event.stopPropagation();
              suppressClickUntilRef.current = 0;
            }}
            onPointerDown={(event: ReactPointerEvent<HTMLDivElement>) => {
              const target = event.target as Element;
              if (
                !isReorderable ||
                event.button > 0 ||
                pointerDragRef.current ||
                target.closest("input, textarea, select, [contenteditable='true']")
              ) {
                return;
              }
              startPointerDrag(session.session_id, event.pointerId, event.clientY);
            }}
            onPointerMove={(event) => {
              const wasDragging = pointerDragRef.current?.dragging ?? false;
              if (updatePointerDrag(event.clientY, event.pointerId)) {
                if (!wasDragging) {
                  event.currentTarget.setPointerCapture?.(event.pointerId);
                }
                event.preventDefault();
              }
            }}
            onPointerUp={(event) => {
              if (finishPointerDrag(event.clientY, event.pointerId)) {
                event.preventDefault();
                event.stopPropagation();
              }
            }}
            onPointerCancel={(event) => {
              cancelPointerDrag(event.pointerId);
            }}
            onLostPointerCapture={(event) => {
              cancelPointerDrag(event.pointerId);
            }}
          >
            <div className="session-row-heading">
              <div className="session-activity-slot">
                <SessionVisual sessionId={session.session_id} activity={session.activity} />
              </div>
              <div className="session-main">
                {isRenaming ? (
                  <form
                    className="session-rename-form"
                    id={`session-rename-${session.session_id}`}
                    // 重命名表单自己处理输入，避免点击输入框时触发行 attach。
                    onClick={(event) => event.stopPropagation()}
                    onSubmit={(event) => {
                      event.preventDefault();
                      const formData = new FormData(event.currentTarget);
                      const nextName = formData.get("session-name");
                      // 中文注释：提交时直接读当前表单值，避免最后一个按键和点击保存
                      // 落在同一批更新里时，React state 仍停在旧 renameDraft。
                      props.onSaveRename(
                        session.session_id,
                        typeof nextName === "string" ? nextName : props.renameDraft,
                      );
                    }}
                  >
                    <label className="sr-only" htmlFor={`session-name-${session.session_id}`}>
                      {t("sessions.name")}
                    </label>
                    <input
                      id={`session-name-${session.session_id}`}
                      name="session-name"
                      aria-label={t("sessions.name")}
                      value={props.renameDraft}
                      onChange={(event) => {
                        // 中文注释：rename 输入框会和 daemon 状态轮询、session.refresh 并发重渲染。
                        // 这里强制同步提交草稿，避免后台刷新先用旧 draft 回写，吞掉最后一个字符。
                        flushSync(() => {
                          props.onRenameDraftChange(event.target.value);
                        });
                      }}
                      autoFocus
                    />
                  </form>
                ) : (
                  <button
                    type="button"
                    className={["session-open-button", hasNewOutput ? "has-new-output" : ""].filter(Boolean).join(" ")}
                    aria-label={activityLabel ? `${openLabel}, ${activityLabel}` : openLabel}
                    onClick={() => props.onAttach(session.session_id)}
                    onKeyDown={(event) => {
                      if (!props.onReorder || !event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) {
                        return;
                      }
                      event.preventDefault();
                      moveSessionByOffset(session.session_id, event.key === "ArrowUp" ? -1 : 1);
                    }}
                  >
                    <strong>{displayName}</strong>
                  </button>
                )}
              </div>
              <div className="session-actions" aria-label={t("sessions.actions")} onClick={(event) => event.stopPropagation()}>
                {isRenaming ? (
                  <>
                    <button
                      type="submit"
                      className="icon-button"
                      form={`session-rename-${session.session_id}`}
                      aria-label={t("sessions.saveName")}
                      disabled={!props.canSaveRename}
                    >
                      <Check size={15} aria-hidden="true" />
                    </button>
                    <button type="button" className="icon-button" aria-label={t("sessions.cancelRename")} onClick={props.onCancelRename}>
                      <X size={15} aria-hidden="true" />
                    </button>
                  </>
                ) : (
                  <>
                    <button
                      type="button"
                      className="icon-button"
                      aria-label={t("sessions.rename")}
                      onClick={() => props.onStartRename(session.session_id, displayName)}
                    >
                      <Pencil size={15} aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      className="icon-button danger"
                      aria-label={t("sessions.close")}
                      onClick={() => props.onClose(session.session_id)}
                    >
                      <Trash2 size={15} aria-hidden="true" />
                    </button>
                  </>
                )}
              </div>
            </div>
          </div>
        );
      })}
    </section>
  );
}
