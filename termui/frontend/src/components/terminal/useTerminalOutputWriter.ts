import { useCallback, useRef, type MutableRefObject } from "react";
import type { TerminalSize } from "../../protocol/types";
import type { TerminalOutputItem } from "./types";
import { recordTermdDiagnostic } from "../../diagnostics";
import type { TerminalRendererTerminal } from "./renderer";

// 中文注释：relay 下一个持续输出 burst 很容易超过 4MiB；太低的 high-water
// 会让前端主动清屏重连，表现成“下行停住，刷新后又恢复”。这里保留内存保护，
// 但把阈值放到浏览器可以继续分片追赶的量级。
const PENDING_WRITE_HIGH_WATER_BYTES = 64 * 1024 * 1024;
// 单次 Ghostty write 过大时会占用浏览器主线程，连控制 WebSocket 和 relay 页面心跳都会被拖慢。
// 64KB 仍是批量写入，不会退回逐字/逐行渲染，同时给输入和切 session 留出帧间隙。
const MAX_WRITE_BYTES = 64 * 1024;

export interface ActiveTerminalWrite {
  item: TerminalOutputItem;
  offset: number;
  sequenceChecked: boolean;
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
  shouldScrollSnapshotToBottom?: (item: Extract<TerminalOutputItem, { kind: "snapshot" }>) => boolean;
  onSnapshotRendered?: (item: Extract<TerminalOutputItem, { kind: "snapshot" }>) => void;
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
      // 单个权威 snapshot/output 会被下游按 64KiB 分片写入 Ghostty；
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
  const writeFrameRef = useRef<number | undefined>(undefined);
  const needsPostWriteRefreshRef = useRef(false);
  const needsPostWriteScrollBottomRef = useRef(false);
  const snapshotRedrawInProgressRef = useRef(false);

  const clearPendingWriteQueue = useCallback(() => {
    pendingWriteItemsRef.current = [];
    pendingWriteBytesRef.current = 0;
  }, []);

  const invalidateWriterForFullResync = useCallback(() => {
    // 中文注释：Ghostty write 无法取消；推进 generation 后，旧 callback 即使稍后触发
    // 也不能再确认 terminal_seq。App 会用完整 snapshot 重建画面，而不是从旧 seq 续传。
    clearPendingWriteQueue();
    activeWriteRef.current = undefined;
    writeInFlightRef.current = false;
    writeGenerationRef.current += 1;
    if (writeFrameRef.current !== undefined) {
      window.cancelAnimationFrame(writeFrameRef.current);
      writeFrameRef.current = undefined;
    }
    needsPostWriteRefreshRef.current = false;
    needsPostWriteScrollBottomRef.current = false;
    snapshotRedrawInProgressRef.current = false;
  }, [clearPendingWriteQueue]);

  const resetWriterState = useCallback(() => {
    invalidateWriterForFullResync();
    lastTerminalSeqRef.current = undefined;
  }, [invalidateWriterForFullResync]);

  const markWriterNeedsRefreshAndScroll = useCallback(() => {
    needsPostWriteRefreshRef.current = true;
    needsPostWriteScrollBottomRef.current = true;
  }, []);

  const takeOutputIntoPendingQueue = useCallback(() => {
    const items = takeOutput();
    if (items.length === 0) {
      return false;
    }
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
    const markItemRendered = (item: TerminalOutputItem) => {
      if (item.kind === "snapshot") {
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
        if (!options.sameTerminalDimensions(options.terminal, item.size)) {
          options.terminal.resize(item.size.cols, item.size.rows);
        }
        options.terminal.reset();
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
        if ((item.kind === "resize" || item.kind === "exit" || terminalOutputItemBytes(item) === 0) && byteCount > 0) {
          break;
        }

        if (!ensureActiveWriteSequence(active, sequenceCursor)) {
          continue;
        }

        if (item.kind === "resize" || item.kind === "exit" || terminalOutputItemBytes(item) === 0) {
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
    const afterTerminalMutation = () => {
      if (options.isDisposed()) {
        return;
      }
      options.queueCursorReport();
      options.scheduleTerminalScrollPosition();
      const outputQueueIdle = !activeWriteRef.current && pendingWriteItemsRef.current.length === 0;
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
      // 一次轻量 refresh。否则某些 Ghostty 渲染时序会等到下一次输入/resize 才 repaint 尾包。
      if (outputQueueIdle) {
        const bottomScrollGeneration = shouldScrollBottomAfterMutation
          ? options.beginBottomScrollFollow()
          : undefined;
        options.requestTrackedFrame(() => {
          if (shouldScrollBottomAfterMutation && bottomScrollGeneration !== undefined) {
            if (options.isBottomScrollFollowActive(bottomScrollGeneration)) {
              options.scrollToBottom(bottomScrollGeneration);
              options.scheduleScrollToBottom(bottomScrollGeneration, 4);
            }
          }
          options.terminal.refresh(0, Math.max(0, options.terminal.rows - 1));
          // 中文注释：真实 Ghostty 的 scrollback/baseY 可能到 write callback 后下一帧才稳定；
          // 这里强制刷新滚动条状态，避免大量输出后侧边滚动条仍停留在“不可用”。
          options.scheduleTerminalScrollPosition({ immediate: true });
          // 切换 session 后浏览器布局和 Ghostty renderer 可能比 write callback 再晚一帧可绘制。
          // 队列已经 idle 时补第二帧刷新，不会放大持续输出路径的绘制压力。
          options.requestTrackedFrame(() => {
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
          // 中文注释：resize/exit 这类零字节 terminal frame 不会触发 Ghostty write callback。
          // 但 resize 已经改变了 Ghostty buffer/viewport，仍必须执行 terminal mutation 完成后的刷新和贴底收尾。
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
        afterTerminalMutation();
        if (activeWriteRef.current || pendingWriteItemsRef.current.length > 0) {
          schedulePendingWrite();
        }
      });
    };
    function schedulePendingWrite() {
      if (writeInFlightRef.current || writeFrameRef.current !== undefined) {
        return;
      }
      writeFrameRef.current = options.requestTrackedFrame(() => {
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
  }, [takeOutputIntoPendingQueue]);

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
    clearPendingWriteQueue,
    invalidateWriterForFullResync,
    resetWriterState,
    markWriterNeedsRefreshAndScroll,
    createTerminalOutputDrain,
    takeOutputIntoPendingQueue,
    terminalOutputItemBytes,
  };
}
