import { useCallback, useEffect, useRef, type MutableRefObject } from "react";
import type { TerminalSize } from "../../protocol/types";
import type { TerminalOutputItem } from "./types";
import { recordTermdDiagnostic } from "../../diagnostics";
import type { TerminalRendererTerminal } from "./renderer";

// 中文注释：relay 下一个持续输出 burst 很容易超过 4MiB；太低的 high-water
// 会让前端主动清屏重连，表现成“下行停住，刷新后又恢复”。这里保留内存保护，
// 但把阈值放到浏览器可以继续分片追赶的量级。
const PENDING_WRITE_HIGH_WATER_BYTES = 64 * 1024 * 1024;
// 单次 xterm write 过大时会占用浏览器主线程，连控制 WebSocket 和 relay 页面心跳都会被拖慢。
// 64KB 仍是批量写入，不会退回逐字/逐行渲染，同时给输入和切 session 留出帧间隙。
const MAX_WRITE_BYTES = 64 * 1024;

export interface ActiveTerminalWrite {
  item: TerminalOutputItem;
  offset: number;
  sequenceChecked: boolean;
}

interface ScheduledWriteHandle {
  cancel: () => void;
  rescueHidden: (force?: boolean) => void;
}

interface TerminalWriteBatch {
  bytes: Uint8Array;
  renderedItems: TerminalOutputItem[];
}

interface CreateTerminalOutputDrainOptions {
  terminal: TerminalRendererTerminal;
  sessionSizeRef: MutableRefObject<TerminalSize | undefined>;
  isDisposed: () => boolean;
  requestTrackedFrame: (callback: () => void) => number;
  cancelTrackedFrame: (frameId: number | undefined) => void;
  sameTerminalDimensions: (a: { rows: number; cols: number } | undefined, b: { rows: number; cols: number } | undefined) => boolean;
  isTerminalPinnedToBottom: (terminal: TerminalRendererTerminal) => boolean;
  scrollToBottom: (generation: number) => void;
  scheduleScrollToBottom: (generation: number, passes?: number) => void;
  queueCursorReport: (options?: { immediate?: boolean }) => void;
  scheduleTerminalScrollPosition: (options?: { immediate?: boolean }) => void;
  beginBottomScrollFollow: () => number;
  isBottomScrollFollowActive: (generation: number) => boolean;
  onTerminalResync: (lastTerminalSeq?: number) => void;
  onTerminalSeqRendered: (terminalSeq: number) => void;
  onTerminalSizeRendered: (size: TerminalSize) => void;
  onTerminalOutputRendered?: (item: TerminalOutputItem) => void;
  onTerminalOutputIdle?: () => void;
  onSnapshotRedrawBegin?: (size: TerminalSize) => void;
  hasTerminalInputFocus?: () => boolean;
  restoreTerminalInputFocus?: () => void;
  shouldScrollSnapshotToBottom?: (item: Extract<TerminalOutputItem, { kind: "snapshot" }>) => boolean;
  onSnapshotRendered?: (item: Extract<TerminalOutputItem, { kind: "snapshot" }>) => void;
}

interface DeferredTerminalFrameTestHook {
  schedule: (callback: () => void) => number;
  cancel: (handle: number) => void;
}

function terminalWriteFrameTestHook(): DeferredTerminalFrameTestHook | undefined {
  return (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: DeferredTerminalFrameTestHook;
  }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
}

function isDocumentAnimationFrameUnsafe(): boolean {
  return typeof document !== "undefined" && (
    document.visibilityState === "hidden" ||
    (typeof document.hasFocus === "function" && !document.hasFocus())
  );
}

function terminalOutputItemBytes(item: TerminalOutputItem): number {
  return item.kind === "data" || item.kind === "snapshot" || item.kind === "output"
    ? item.bytes.byteLength
    : 0;
}

function highWaterBacklogBytes(items: TerminalOutputItem[]): number {
  let oversizedFrameAllowance = 1;
  let total = 0;
  for (const item of items) {
    const bytes = terminalOutputItemBytes(item);
    if (bytes > PENDING_WRITE_HIGH_WATER_BYTES && oversizedFrameAllowance > 0) {
      // 单个权威 snapshot/output 会被下游按 64KiB 分片写入 xterm；
      // high-water 只处理累计 backlog，不能因为这个单帧本身反复触发 full resync。
      oversizedFrameAllowance -= 1;
      continue;
    }
    total += bytes;
  }
  return total;
}

function concatWriteChunks(chunks: Uint8Array[], totalBytes: number): Uint8Array {
  if (chunks.length === 1 && chunks[0].byteLength === totalBytes) {
    return chunks[0];
  }
  const merged = new Uint8Array(totalBytes);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return merged;
}

export function useTerminalOutputWriter(
  takeOutput: () => TerminalOutputItem[],
  onHighWaterResync: () => void,
) {
  const pendingWriteItemsRef = useRef<TerminalOutputItem[]>([]);
  const pendingWriteBytesRef = useRef(0);
  const activeWriteRef = useRef<ActiveTerminalWrite | undefined>(undefined);
  const lastTerminalSeqRef = useRef<number | undefined>(undefined);
  const writeInFlightRef = useRef(false);
  const writeGenerationRef = useRef(0);
  const writeFrameRef = useRef<ScheduledWriteHandle | undefined>(undefined);
  const needsPostWriteRefreshRef = useRef(false);
  const needsPostWriteScrollBottomRef = useRef(false);
  const snapshotRedrawInProgressRef = useRef(false);
  const idleRefreshEpochRef = useRef(0);
  const cancelTrackedFrameRef = useRef<(frameId: number | undefined) => void>(() => undefined);
  const idleRefreshFrameRef = useRef<number | undefined>(undefined);
  const idleRefreshFollowupFrameRef = useRef<number | undefined>(undefined);

  const cancelPendingIdleRefreshFrames = useCallback(() => {
    // 中文注释：attach/reconnect 的尾帧 refresh 可能比后续 burst 输出更晚执行。
    // 一旦新输出重新入队，旧 refresh 就已经过期；这里直接撤销，避免把前一个
    // idle 周期的刷新计入新的持续输出路径。
    cancelTrackedFrameRef.current(idleRefreshFrameRef.current);
    idleRefreshFrameRef.current = undefined;
    cancelTrackedFrameRef.current(idleRefreshFollowupFrameRef.current);
    idleRefreshFollowupFrameRef.current = undefined;
  }, []);

  const clearPendingWriteQueue = useCallback(() => {
    pendingWriteItemsRef.current = [];
    pendingWriteBytesRef.current = 0;
  }, []);

  const invalidateWriterForFullResync = useCallback(() => {
    // 中文注释：xterm write 无法取消；推进 generation 后，旧 callback 即使稍后触发
    // 也不能再确认 terminal_seq。App 会用完整 snapshot 重建画面，而不是从旧 seq 续传。
    clearPendingWriteQueue();
    activeWriteRef.current = undefined;
    writeInFlightRef.current = false;
    writeGenerationRef.current += 1;
    if (writeFrameRef.current !== undefined) {
      writeFrameRef.current.cancel();
      writeFrameRef.current = undefined;
    }
    cancelPendingIdleRefreshFrames();
    needsPostWriteRefreshRef.current = false;
    needsPostWriteScrollBottomRef.current = false;
    snapshotRedrawInProgressRef.current = false;
  }, [cancelPendingIdleRefreshFrames, clearPendingWriteQueue]);

  const resetWriterState = useCallback(() => {
    invalidateWriterForFullResync();
    lastTerminalSeqRef.current = undefined;
  }, [invalidateWriterForFullResync]);

  const markWriterNeedsRefreshAndScroll = useCallback(() => {
    needsPostWriteRefreshRef.current = true;
    needsPostWriteScrollBottomRef.current = true;
  }, []);

  const scheduleWriteCallback = useCallback((callback: () => void): ScheduledWriteHandle => {
    const writeFrameTestHook = terminalWriteFrameTestHook();
    const pageUnsafeAtSchedule = isDocumentAnimationFrameUnsafe();
    let frame: number | undefined;
    let timer: number | undefined;
    let settled = false;
    const clearScheduled = () => {
      if (frame !== undefined) {
        if (writeFrameTestHook) {
          writeFrameTestHook.cancel(frame);
        } else {
          window.cancelAnimationFrame(frame);
        }
        frame = undefined;
      }
      if (timer !== undefined) {
        window.clearTimeout(timer);
        timer = undefined;
      }
    };
    const run = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearScheduled();
      callback();
    };
    const armTimer = () => {
      if (settled || timer !== undefined) {
        return;
      }
      timer = window.setTimeout(() => {
        run();
      }, 0);
    };
    if (pageUnsafeAtSchedule) {
      armTimer();
      return {
        cancel: () => {
          settled = true;
          clearScheduled();
        },
        rescueHidden: () => undefined,
      };
    }
    if (writeFrameTestHook) {
      frame = writeFrameTestHook.schedule(() => {
        run();
      });
    } else {
      frame = window.requestAnimationFrame(() => {
        run();
      });
    }
    return {
      cancel: () => {
        settled = true;
        clearScheduled();
      },
      rescueHidden: (force = false) => {
        if (settled || frame === undefined) {
          return;
        }
        if (!force && !isDocumentAnimationFrameUnsafe()) {
          return;
        }
        // 中文注释：write drain 如果先按前台路径挂进 rAF，随后标签页立刻 hidden，
        // 或者窗口先 blur、hidden 稍后才到，这个 callback 也可能被浏览器暂停。
        // 这里把 pending write 改挂到 timer，
        // 让 xterm 继续消费 backlog，不依赖前台动画帧。
        if (writeFrameTestHook) {
          writeFrameTestHook.cancel(frame);
        } else {
          window.cancelAnimationFrame(frame);
        }
        frame = undefined;
        armTimer();
      },
    };
  }, []);

  useEffect(() => {
    if (typeof document === "undefined") {
      return undefined;
    }
    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        writeFrameRef.current?.rescueHidden(true);
      }
    };
    const handleWindowBlur = () => {
      writeFrameRef.current?.rescueHidden(true);
    };
    document.addEventListener("visibilitychange", handleVisibilityChange);
    window.addEventListener("blur", handleWindowBlur);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      window.removeEventListener("blur", handleWindowBlur);
    };
  }, []);

  const takeOutputIntoPendingQueue = useCallback(() => {
    const items = takeOutput();
    if (items.length === 0) {
      return false;
    }
    // 中文注释：只要新输出进队，之前按“队列已空”排队的 idle refresh 就可能过期。
    // 用 epoch 把这些 refresh 变成可失效任务，避免持续输出时每个短暂空窗都真的刷新两帧。
    idleRefreshEpochRef.current += 1;
    cancelPendingIdleRefreshFrames();
    pendingWriteItemsRef.current.push(...items);
    pendingWriteBytesRef.current += items.reduce((sum, item) => sum + terminalOutputItemBytes(item), 0);
    if (highWaterBacklogBytes(pendingWriteItemsRef.current) > PENDING_WRITE_HIGH_WATER_BYTES) {
      // 中文注释：renderer 如果长期追不上 daemon 输出，继续堆积只会放大内存和延迟。
      // 这里必须同时废弃 active write 和旧 callback generation，否则旧 write 完成后
      // 可能在 resync 之后错误推进 terminal_seq。
      recordTermdDiagnostic("terminal_writer_high_water", {
        pendingItems: pendingWriteItemsRef.current.length,
        pendingBytes: pendingWriteBytesRef.current,
        highWaterBytes: PENDING_WRITE_HIGH_WATER_BYTES,
      });
      invalidateWriterForFullResync();
      onHighWaterResync();
      return false;
    }
    return true;
  }, [invalidateWriterForFullResync, onHighWaterResync, takeOutput]);

  const createTerminalOutputDrain = useCallback((options: CreateTerminalOutputDrainOptions) => {
    cancelTrackedFrameRef.current = options.cancelTrackedFrame;
    const markItemRendered = (item: TerminalOutputItem) => {
      if (item.kind === "sync") {
        lastTerminalSeqRef.current = item.baseSeq;
        options.onTerminalSeqRendered(item.baseSeq);
      } else if (item.kind === "snapshot") {
        snapshotRedrawInProgressRef.current = false;
        lastTerminalSeqRef.current = item.baseSeq;
        options.onTerminalSeqRendered(item.baseSeq);
        recordTermdDiagnostic("terminal_writer_snapshot_rendered", {
          baseSeq: item.baseSeq,
          bytes: item.bytes.byteLength,
          pendingItems: pendingWriteItemsRef.current.length,
          pendingBytes: pendingWriteBytesRef.current,
        });
        // 中文注释：snapshot 必须按生成时的 PTY 尺寸重放；但重放完成后要把当前
        // 聚焦客户端重新 fit 到真实容器尺寸，否则后续输入会先按旧 24 行滚动，
        // 再被 resize 扩成大屏，表现为内容悬在上半截。
        options.onSnapshotRendered?.(item);
      } else if (item.kind === "output" || item.kind === "resize" || item.kind === "exit") {
        lastTerminalSeqRef.current = item.terminalSeq;
        options.onTerminalSeqRendered(item.terminalSeq);
      }
      options.onTerminalOutputRendered?.(item);
    };
    const advanceSequenceCursor = (item: TerminalOutputItem, current: number | undefined) => {
      if (item.kind === "sync") {
        return item.baseSeq;
      }
      if (item.kind === "snapshot") {
        return item.baseSeq;
      }
      if (item.kind === "output" || item.kind === "resize" || item.kind === "exit") {
        return item.terminalSeq;
      }
      return current;
    };
    const ensureActiveWriteSequence = (active: ActiveTerminalWrite, sequenceCursor = lastTerminalSeqRef.current): boolean => {
      if (active.sequenceChecked) {
        return true;
      }
      active.sequenceChecked = true;
      const { item } = active;
      if (item.kind === "sync") {
        return true;
      }
      if (item.kind === "snapshot") {
        snapshotRedrawInProgressRef.current = true;
        options.sessionSizeRef.current = item.size;
        recordTermdDiagnostic("terminal_writer_snapshot_begin", {
          baseSeq: item.baseSeq,
          bytes: item.bytes.byteLength,
          size: item.size,
          pendingItems: pendingWriteItemsRef.current.length,
          pendingBytes: pendingWriteBytesRef.current,
        });
        // 中文注释：snapshot 必须临时按 daemon 生成时的 rows/cols 解析历史。
        // 这一步如果直接展示给用户，会在 reload 时看到 80x24 -> 本地尺寸的闪烁。
        options.onSnapshotRedrawBegin?.(item.size);
        const shouldRestoreTerminalInputFocus = options.hasTerminalInputFocus?.() ?? false;
        if (!options.sameTerminalDimensions(options.terminal, item.size)) {
          options.terminal.resize(item.size.cols, item.size.rows);
        }
        options.terminal.reset();
        if (shouldRestoreTerminalInputFocus) {
          // 中文注释：首个 attach snapshot 也会 reset xterm；若 reset 前输入焦点已经在终端内，
          // 需要立即恢复同一个 helper textarea，避免首屏闪一下后留在失焦状态。
          options.restoreTerminalInputFocus?.();
        }
        needsPostWriteRefreshRef.current = true;
        // 中文注释：普通 attach/reload 的 snapshot 写入可能晚于初期 resize/stabilize，
        // 写完后必须再贴底；但用户主动向上滚触发的历史 snapshot 不能贴底，
        // 否则第一次滚轮只会拉历史却看不到历史。
        needsPostWriteScrollBottomRef.current = options.shouldScrollSnapshotToBottom?.(item) ?? true;
        return true;
      }
      if (item.kind === "output" || item.kind === "resize" || item.kind === "exit") {
        const expected = (sequenceCursor ?? -1) + 1;
        if (sequenceCursor === undefined || item.terminalSeq !== expected) {
          // 中文注释：terminal_seq 缺口说明 snapshot/tail 已经不连续，必须重新 attach 获取权威 snapshot。
          recordTermdDiagnostic("terminal_writer_sequence_gap", {
            sequenceCursor,
            expected,
            actual: item.terminalSeq,
            kind: item.kind,
            pendingItems: pendingWriteItemsRef.current.length,
            pendingBytes: pendingWriteBytesRef.current,
          });
          options.onTerminalResync(sequenceCursor);
          activeWriteRef.current = undefined;
          return false;
        }
      }
      return true;
    };
    const applyResizeFrame = (item: Extract<TerminalOutputItem, { kind: "resize" }>) => {
      const wasPinnedToBottom = options.isTerminalPinnedToBottom(options.terminal);
      options.sessionSizeRef.current = item.size;
      if (!options.sameTerminalDimensions(options.terminal, item.size)) {
        options.terminal.resize(item.size.cols, item.size.rows);
      }
      options.onTerminalSizeRendered(item.size);
      needsPostWriteRefreshRef.current = true;
      needsPostWriteScrollBottomRef.current = needsPostWriteScrollBottomRef.current || wasPinnedToBottom;
      options.queueCursorReport({ immediate: true });
    };
    const takePendingWrite = (): TerminalWriteBatch | undefined => {
      const chunks: Uint8Array[] = [];
      const renderedItems: TerminalOutputItem[] = [];
      let byteCount = 0;
      let sequenceCursor = lastTerminalSeqRef.current;

      while (byteCount < MAX_WRITE_BYTES) {
        let active = activeWriteRef.current;
        if (!active) {
          const item = pendingWriteItemsRef.current.shift();
          if (!item) {
            break;
          }
          pendingWriteBytesRef.current = Math.max(0, pendingWriteBytesRef.current - terminalOutputItemBytes(item));
          active = { item, offset: 0, sequenceChecked: false };
          activeWriteRef.current = active;
        }

        const { item } = active;
        if ((item.kind === "sync" || item.kind === "resize" || item.kind === "exit" || terminalOutputItemBytes(item) === 0) && byteCount > 0) {
          break;
        }

        if (!ensureActiveWriteSequence(active, sequenceCursor)) {
          continue;
        }

        if (item.kind === "sync" || item.kind === "resize" || item.kind === "exit" || terminalOutputItemBytes(item) === 0) {
          if (item.kind === "resize") {
            applyResizeFrame(item);
          }
          renderedItems.push(item);
          sequenceCursor = advanceSequenceCursor(item, sequenceCursor);
          activeWriteRef.current = undefined;
          continue;
        }

        if (item.kind !== "data" && item.kind !== "snapshot" && item.kind !== "output") {
          break;
        }

        const remaining = MAX_WRITE_BYTES - byteCount;
        const end = Math.min(item.bytes.byteLength, active.offset + remaining);
        const slice = item.bytes.subarray(active.offset, end);
        chunks.push(slice);
        byteCount += slice.byteLength;
        active.offset = end;

        if (active.offset >= terminalOutputItemBytes(item)) {
          renderedItems.push(item);
          sequenceCursor = advanceSequenceCursor(item, sequenceCursor);
          activeWriteRef.current = undefined;
          continue;
        }

        break;
      }

      if (byteCount === 0) {
        for (const item of renderedItems) {
          markItemRendered(item);
        }
        return undefined;
      }

      return {
        bytes: concatWriteChunks(chunks, byteCount),
        renderedItems,
      };
    };
    const afterTerminalMutation = (renderedItems: TerminalOutputItem[] = []) => {
      if (options.isDisposed()) {
        return;
      }
      options.queueCursorReport();
      options.scheduleTerminalScrollPosition();
      const outputQueueIdle = !activeWriteRef.current && pendingWriteItemsRef.current.length === 0;
      const requiresBusyRefresh = renderedItems.some((item) =>
        item.kind === "snapshot" || item.kind === "resize" || item.kind === "exit",
      );
      if (!needsPostWriteRefreshRef.current && !outputQueueIdle) {
        return;
      }
      const shouldScrollBottomAfterMutation = outputQueueIdle && needsPostWriteScrollBottomRef.current;
      if (outputQueueIdle) {
        options.onTerminalOutputIdle?.();
      }
      needsPostWriteRefreshRef.current = false;
      if (shouldScrollBottomAfterMutation) {
        needsPostWriteScrollBottomRef.current = false;
      }
      // 首屏/清屏后的首个 write，以及 resize/exit 这类零字节 mutation，都需要
      // 一次轻量 refresh。否则某些 xterm 渲染时序会等到下一次输入/resize 才 repaint 尾包。
      if (outputQueueIdle) {
        const idleRefreshEpoch = idleRefreshEpochRef.current;
        const bottomScrollGeneration = shouldScrollBottomAfterMutation
          ? options.beginBottomScrollFollow()
          : undefined;
        idleRefreshFrameRef.current = options.requestTrackedFrame(() => {
          idleRefreshFrameRef.current = undefined;
          if (
            idleRefreshEpoch !== idleRefreshEpochRef.current ||
            writeInFlightRef.current ||
            activeWriteRef.current ||
            pendingWriteItemsRef.current.length > 0
          ) {
            return;
          }
          if (shouldScrollBottomAfterMutation && bottomScrollGeneration !== undefined) {
            if (options.isBottomScrollFollowActive(bottomScrollGeneration)) {
              options.scrollToBottom(bottomScrollGeneration);
              options.scheduleScrollToBottom(bottomScrollGeneration, 4);
            }
          }
          options.terminal.refresh(0, Math.max(0, options.terminal.rows - 1));
          // 中文注释：真实 xterm 的 scrollback/baseY 可能到 write callback 后下一帧才稳定；
          // 这里强制刷新滚动条状态，避免大量输出后侧边滚动条仍停留在“不可用”。
          options.scheduleTerminalScrollPosition({ immediate: true });
          // 切换 session 后浏览器布局和 xterm renderer 可能比 write callback 再晚一帧可绘制。
          // 队列已经 idle 时补第二帧刷新，不会放大持续输出路径的绘制压力。
          idleRefreshFollowupFrameRef.current = options.requestTrackedFrame(() => {
            idleRefreshFollowupFrameRef.current = undefined;
            if (
              idleRefreshEpoch !== idleRefreshEpochRef.current ||
              writeInFlightRef.current ||
              activeWriteRef.current ||
              pendingWriteItemsRef.current.length > 0
            ) {
              return;
            }
            if (shouldScrollBottomAfterMutation && bottomScrollGeneration !== undefined) {
              if (options.isTerminalPinnedToBottom(options.terminal)) {
                if (options.isBottomScrollFollowActive(bottomScrollGeneration)) {
                  options.scrollToBottom(bottomScrollGeneration);
                }
              }
            }
            options.terminal.refresh(0, Math.max(0, options.terminal.rows - 1));
            options.scheduleTerminalScrollPosition({ immediate: true });
          });
        });
        return;
      }
      if (!requiresBusyRefresh) {
        return;
      }
      // 持续输出路径不反复 proposeDimensions/refresh，降低 layout 和绘制压力。
      options.requestTrackedFrame(() => {
        options.terminal.refresh(0, Math.max(0, options.terminal.rows - 1));
        options.scheduleTerminalScrollPosition({ immediate: true });
      });
    };
    const flushPendingWrite = () => {
      if (writeInFlightRef.current) {
        return;
      }
      const output = takePendingWrite();
      if (!output || output.bytes.byteLength === 0) {
        if (needsPostWriteRefreshRef.current || needsPostWriteScrollBottomRef.current) {
          // 中文注释：resize/exit 这类零字节 terminal frame 不会触发 xterm write callback。
          // 但 resize 已经改变了 xterm buffer/viewport，仍必须执行 terminal mutation 完成后的刷新和贴底收尾。
          afterTerminalMutation();
        }
        if (activeWriteRef.current || pendingWriteItemsRef.current.length > 0) {
          schedulePendingWrite();
        }
        return;
      }
      writeInFlightRef.current = true;
      const writeGeneration = writeGenerationRef.current;
      options.terminal.write(output.bytes, () => {
        if (options.isDisposed() || writeGeneration !== writeGenerationRef.current) {
          return;
        }
        writeInFlightRef.current = false;
        for (const item of output.renderedItems) {
          markItemRendered(item);
        }
        afterTerminalMutation(output.renderedItems);
        if (activeWriteRef.current || pendingWriteItemsRef.current.length > 0) {
          schedulePendingWrite();
        }
      });
    };
    function schedulePendingWrite() {
      if (writeInFlightRef.current || writeFrameRef.current !== undefined) {
        return;
      }
      writeFrameRef.current = scheduleWriteCallback(() => {
        if (options.isDisposed()) {
          return;
        }
        writeFrameRef.current = undefined;
        flushPendingWrite();
      });
    }
    return () => {
      if (!takeOutputIntoPendingQueue()) {
        return;
      }
      schedulePendingWrite();
    };
  }, [scheduleWriteCallback, takeOutputIntoPendingQueue]);

  return {
    pendingWriteItemsRef,
    pendingWriteBytesRef,
    activeWriteRef,
    lastTerminalSeqRef,
    writeInFlightRef,
    writeGenerationRef,
    writeFrameRef,
    needsPostWriteRefreshRef,
    needsPostWriteScrollBottomRef,
    snapshotRedrawInProgressRef,
    idleRefreshEpochRef,
    clearPendingWriteQueue,
    invalidateWriterForFullResync,
    resetWriterState,
    markWriterNeedsRefreshAndScroll,
    createTerminalOutputDrain,
    takeOutputIntoPendingQueue,
    terminalOutputItemBytes,
  };
}
