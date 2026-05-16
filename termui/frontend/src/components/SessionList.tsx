import { Check, GripVertical, Pencil, Trash2, X } from "lucide-react";
import {
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import type { SessionSummaryPayload, UUID } from "../protocol/types";
import { sessionDisplayName } from "../session-names";
import { useI18n } from "../i18n";

interface SessionListProps {
  sessions: SessionSummaryPayload[];
  selectedSessionId?: UUID;
  newOutputSessionIds?: ReadonlySet<UUID>;
  renamingSessionId?: UUID;
  renameDraft: string;
  canSaveRename: boolean;
  onAttach: (sessionId: UUID) => void;
  onStartRename: (sessionId: UUID, currentName: string) => void;
  onRenameDraftChange: (name: string) => void;
  onSaveRename: (sessionId: UUID) => void;
  onCancelRename: () => void;
  onClose: (sessionId: UUID) => void;
  onReorder?: (sessionIds: UUID[]) => void;
}

export function SessionList(props: SessionListProps) {
  const [draggingSessionId, setDraggingSessionId] = useState<UUID | undefined>();
  const [dragTargetSessionId, setDragTargetSessionId] = useState<UUID | undefined>();
  const { t } = useI18n();
  const rowRefs = useRef(new Map<UUID, HTMLDivElement>());
  const pointerDragRef = useRef<{
    pointerId: number;
    sessionId: UUID;
    targetSessionId: UUID;
  } | null>(null);

  const isActivePointer = (eventPointerId: number | undefined, dragPointerId: number) =>
    eventPointerId === undefined || eventPointerId === 0 || eventPointerId === dragPointerId;

  const startPointerDrag = (sessionId: UUID, pointerId: number, clientY: number) => {
    pointerDragRef.current = {
      pointerId: pointerId || 1,
      sessionId,
      targetSessionId: resolvePointerTarget(clientY).sessionId ?? sessionId,
    };
    setDraggingSessionId(sessionId);
    setDragTargetSessionId(pointerDragRef.current.targetSessionId);
  };

  const updatePointerDrag = (clientY: number, pointerId?: number) => {
    const drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return;
    }
    const target = resolvePointerTarget(clientY);
    if (target.sessionId) {
      pointerDragRef.current = { ...drag, targetSessionId: target.sessionId };
      setDragTargetSessionId(target.sessionId);
    }
  };

  const finishPointerDrag = (clientY: number, pointerId?: number) => {
    const drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return;
    }
    const target = resolvePointerTarget(clientY);
    pointerDragRef.current = null;
    setDraggingSessionId(undefined);
    setDragTargetSessionId(undefined);
    moveSessionToIndex(drag.sessionId, target.index);
  };

  const cancelPointerDrag = (pointerId?: number) => {
    const drag = pointerDragRef.current;
    if (!drag || !isActivePointer(pointerId, drag.pointerId)) {
      return;
    }
    pointerDragRef.current = null;
    setDraggingSessionId(undefined);
    setDragTargetSessionId(undefined);
  };

  const moveSessionBefore = (targetSessionId: UUID) => {
    if (!draggingSessionId || draggingSessionId === targetSessionId) {
      return;
    }
    const currentIds = props.sessions.map((session) => session.session_id);
    const withoutDragging = currentIds.filter((sessionId) => sessionId !== draggingSessionId);
    const targetIndex = withoutDragging.indexOf(targetSessionId);
    if (targetIndex < 0) {
      return;
    }
    const next = [...withoutDragging];
    next.splice(targetIndex, 0, draggingSessionId);
    props.onReorder?.(next);
  };

  const moveSessionToIndex = (sessionId: UUID, targetIndex: number) => {
    const currentIds = props.sessions.map((session) => session.session_id);
    const withoutDragging = currentIds.filter((candidateId) => candidateId !== sessionId);
    const clampedIndex = Math.max(0, Math.min(targetIndex, withoutDragging.length));
    const next = [...withoutDragging];
    next.splice(clampedIndex, 0, sessionId);
    props.onReorder?.(next);
  };

  const resolvePointerTarget = (clientY: number) => {
    const rows = props.sessions
      .map((session) => ({
        sessionId: session.session_id,
        rect: rowRefs.current.get(session.session_id)?.getBoundingClientRect(),
      }))
      .filter((entry): entry is { sessionId: UUID; rect: DOMRect } => Boolean(entry.rect));

    for (let index = 0; index < rows.length; index += 1) {
      const row = rows[index];
      if (clientY < row.rect.top + row.rect.height / 2) {
        return { sessionId: row.sessionId, index };
      }
    }

    return { sessionId: rows.at(-1)?.sessionId, index: rows.length };
  };

  useEffect(() => {
    if (typeof window === "undefined") {
      return undefined;
    }

    const handlePointerMove = (event: PointerEvent) => {
      updatePointerDrag(event.clientY, event.pointerId);
    };

    const handlePointerUp = (event: PointerEvent) => finishPointerDrag(event.clientY, event.pointerId);
    const handlePointerCancel = (event: PointerEvent) => cancelPointerDrag(event.pointerId);

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp);
    window.addEventListener("pointercancel", handlePointerCancel);
    return () => {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
      window.removeEventListener("pointercancel", handlePointerCancel);
    };
  });

  return (
    <section className="session-list" aria-label={t("sessions.aria")}>
      {props.sessions.length === 0 ? <div className="empty-list">{t("sessions.empty")}</div> : null}
      {props.sessions.map((session) => {
        const displayName = sessionDisplayName(session);
        const isRenaming = props.renamingSessionId === session.session_id;
        const hasNewOutput = props.newOutputSessionIds?.has(session.session_id) ?? false;
        const isDragTarget =
          Boolean(draggingSessionId) &&
          draggingSessionId !== session.session_id &&
          dragTargetSessionId === session.session_id;
        return (
          <div
            className={[
              "session-row",
              session.session_id === props.selectedSessionId ? "selected" : "",
              hasNewOutput ? "has-new-output" : "",
              draggingSessionId === session.session_id ? "dragging" : "",
              isDragTarget ? "drag-target" : "",
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
            role="button"
            tabIndex={0}
            aria-label={hasNewOutput ? t("sessions.openNewOutput", { name: displayName }) : t("sessions.open", { name: displayName })}
            onDragOver={(event) => {
              if (!draggingSessionId || draggingSessionId === session.session_id) {
                return;
              }
              event.preventDefault();
              setDragTargetSessionId(session.session_id);
            }}
            onDrop={(event) => {
              event.preventDefault();
              moveSessionBefore(session.session_id);
              setDraggingSessionId(undefined);
              setDragTargetSessionId(undefined);
            }}
            onClick={() => props.onAttach(session.session_id)}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) {
                return;
              }
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                props.onAttach(session.session_id);
              }
            }}
          >
            <div className="session-row-heading">
              <button
                type="button"
                className="icon-button session-drag-handle"
                aria-label={t("sessions.drag", { name: displayName })}
                draggable={Boolean(props.onReorder)}
                onClick={(event) => event.stopPropagation()}
                onPointerDown={(event: ReactPointerEvent<HTMLButtonElement>) => {
                  if (!props.onReorder || event.button > 0) {
                    return;
                  }
                  // 只从手柄启动 pointer 排序，避免和打开 session 的整行点击冲突。
                  event.preventDefault();
                  event.stopPropagation();
                  startPointerDrag(session.session_id, event.pointerId, event.clientY);
                  event.currentTarget.setPointerCapture?.(event.pointerId);
                }}
                onPointerMove={(event) => {
                  updatePointerDrag(event.clientY, event.pointerId);
                }}
                onPointerUp={(event) => {
                  finishPointerDrag(event.clientY, event.pointerId);
                }}
                onPointerCancel={(event) => {
                  cancelPointerDrag(event.pointerId);
                }}
                onMouseDown={(event: ReactMouseEvent<HTMLButtonElement>) => {
                  if (!props.onReorder || event.button !== 0 || pointerDragRef.current) {
                    return;
                  }
                  event.preventDefault();
                  event.stopPropagation();
                  startPointerDrag(session.session_id, 1, event.clientY);
                }}
                onMouseMove={(event) => {
                  updatePointerDrag(event.clientY, 1);
                }}
                onMouseUp={(event) => {
                  finishPointerDrag(event.clientY, 1);
                }}
                onDragStart={(event) => {
                  event.stopPropagation();
                  setDraggingSessionId(session.session_id);
                  setDragTargetSessionId(session.session_id);
                  event.dataTransfer.effectAllowed = "move";
                  event.dataTransfer.setData("text/plain", session.session_id);
                }}
                onDragEnd={() => {
                  setDraggingSessionId(undefined);
                  setDragTargetSessionId(undefined);
                }}
                onKeyDown={(event) => {
                  if (!props.onReorder || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) {
                    return;
                  }
                  event.preventDefault();
                  event.stopPropagation();
                  const ids = props.sessions.map((candidate) => candidate.session_id);
                  const index = ids.indexOf(session.session_id);
                  const nextIndex = event.key === "ArrowUp" ? index - 1 : index + 1;
                  if (index < 0 || nextIndex < 0 || nextIndex >= ids.length) {
                    return;
                  }
                  const next = [...ids];
                  [next[index], next[nextIndex]] = [next[nextIndex], next[index]];
                  props.onReorder(next);
                }}
              >
                <GripVertical size={15} aria-hidden="true" />
              </button>
              <div className="session-main">
                {isRenaming ? (
                  <form
                    className="session-rename-form"
                    id={`session-rename-${session.session_id}`}
                    // 重命名表单自己处理输入，避免点击输入框时触发行 attach。
                    onClick={(event) => event.stopPropagation()}
                    onSubmit={(event) => {
                      event.preventDefault();
                      props.onSaveRename(session.session_id);
                    }}
                  >
                    <label className="sr-only" htmlFor={`session-name-${session.session_id}`}>
                      {t("sessions.name")}
                    </label>
                    <input
                      id={`session-name-${session.session_id}`}
                      aria-label={t("sessions.name")}
                      value={props.renameDraft}
                      onChange={(event) => props.onRenameDraftChange(event.target.value)}
                      autoFocus
                    />
                  </form>
                ) : (
                  <strong>{displayName}</strong>
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
