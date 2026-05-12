import { useEffect, useRef, useState, type MouseEvent } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Maximize2, RotateCcw, ZoomIn, ZoomOut } from "lucide-react";
import type { SessionCursorPresence, TerminalSize } from "../protocol/types";

const TERMINAL_FONT_SIZE = 13;
const TERMINAL_PADDING_PX = 12;
const TERMINAL_FRAME_BORDER_PX = 1;
const TERMINAL_FRAME_CHROME_PX = TERMINAL_PADDING_PX * 2 + TERMINAL_FRAME_BORDER_PX * 2;
const TERMINAL_LINE_HEIGHT = 1.45;
const MIN_FOCUSED_RESIZE_ROWS = 6;
const MIN_FOCUSED_RESIZE_COLS = 20;
const VIEWER_ZOOM_STEP = 0.1;
const VIEWER_MIN_ZOOM = 0.5;
const VIEWER_MAX_ZOOM = 1.4;
type ResizeSource = "layout" | "focus" | "viewer";

function sameTerminalDimensions(
  a: { rows: number; cols: number } | undefined,
  b: { rows: number; cols: number } | undefined,
): boolean {
  return Boolean(a) && Boolean(b) && a!.rows === b!.rows && a!.cols === b!.cols;
}

interface TerminalPaneProps {
  attached: boolean;
  sessionSize?: TerminalSize;
  focusRequest?: number;
  outputVersion: number;
  outputResetVersion: number;
  takeOutput: () => string[];
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
  onCursorChange?: (presence: SessionCursorPresence) => void;
}

export function TerminalPane(props: TerminalPaneProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const scrollportRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const outputResetVersionRef = useRef(props.outputResetVersion);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);
  const onCursorChangeRef = useRef(props.onCursorChange);
  const takeOutputRef = useRef(props.takeOutput);
  const sessionSizeRef = useRef(props.sessionSize);
  const viewerScaleRef = useRef(1);
  const resizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const stabilizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const cursorFrameRef = useRef<number | undefined>(undefined);
  const focusedRef = useRef(false);
  const clientSizeRef = useRef<TerminalSize | undefined>(undefined);
  const viewerModeRef = useRef(false);
  const focusActivationArmedRef = useRef(false);
  const suppressPassiveFocusRef = useRef(false);
  const currentFontSizeRef = useRef(TERMINAL_FONT_SIZE);
  const [clientSize, setClientSize] = useState<TerminalSize | undefined>(undefined);
  const [focused, setFocused] = useState(false);
  const [viewerScale, setViewerScale] = useState(1);
  const [writtenChunkCount, setWrittenChunkCount] = useState(0);
  const fitViewerToScrollport = () => setViewerScale(fitScaleForViewer(scrollportRef.current, frameRef.current, viewerScaleRef.current));
  const remoteRenderMode = props.attached && !focused;
  const viewerCols = props.sessionSize?.cols ?? 0;
  const viewerRows = props.sessionSize?.rows ?? 0;
  const viewerPixelWidth = props.sessionSize?.pixel_width ?? 0;
  const viewerPixelHeight = props.sessionSize?.pixel_height ?? 0;
  // 只有 PTY 尺寸和当前客户端可容纳尺寸不一致时，才展示 viewer 的虚线框和缩放工具。
  const resolutionMismatch =
    remoteRenderMode &&
    viewerCols > 0 &&
    viewerRows > 0 &&
    clientSize !== undefined &&
    (clientSize.cols !== viewerCols || clientSize.rows !== viewerRows);
  const effectiveViewerScale = resolutionMismatch ? viewerScale : 1;
  const viewerFontSize = fontSizeForScale(effectiveViewerScale);
  const viewerFrameStyle =
    resolutionMismatch && viewerCols > 0 && viewerRows > 0
      ? {
          // 优先使用聚焦端上报的像素尺寸；缺失时才按 rows/cols 估算 PTY 画布。
          width:
            viewerPixelWidth > 0
              ? `${Math.ceil(viewerPixelWidth * effectiveViewerScale) + TERMINAL_FRAME_CHROME_PX}px`
              : `calc(${viewerCols}ch + ${TERMINAL_FRAME_CHROME_PX}px)`,
          height:
            viewerPixelHeight > 0
              ? `${Math.ceil(viewerPixelHeight * effectiveViewerScale) + TERMINAL_FRAME_CHROME_PX}px`
              : `${Math.ceil(viewerRows * viewerFontSize * TERMINAL_LINE_HEIGHT) + TERMINAL_FRAME_CHROME_PX}px`,
          fontSize: `${viewerFontSize}px`,
          fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
        }
      : undefined;

  useEffect(() => {
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onCursorChangeRef.current = props.onCursorChange;
    takeOutputRef.current = props.takeOutput;
    sessionSizeRef.current = props.sessionSize;
  }, [props.onCursorChange, props.onInput, props.onResize, props.sessionSize, props.takeOutput]);

  useEffect(() => {
    viewerScaleRef.current = viewerScale;
    if (!focusedRef.current) {
      resizeRef.current?.("viewer");
    }
  }, [viewerScale]);

  useEffect(() => {
    resizeRef.current?.(focused ? "focus" : "layout");
  }, [focused]);

  useEffect(() => {
    sessionSizeRef.current = props.sessionSize;
    if (!focusedRef.current) {
      resizeRef.current?.("viewer");
    }
  }, [props.sessionSize?.cols, props.sessionSize?.pixel_height, props.sessionSize?.pixel_width, props.sessionSize?.rows]);

  const queueCursorReport = () => {
    if (cursorFrameRef.current !== undefined) {
      return;
    }
    cursorFrameRef.current = window.requestAnimationFrame(() => {
      cursorFrameRef.current = undefined;
      const terminal = terminalRef.current;
      if (!terminal || !onCursorChangeRef.current) {
        return;
      }

      // xterm 内部 cursorX/cursorY 是 0-based；协议用 1-based，便于顶部状态条直接展示。
      // jsdom 测试环境不会完整实现 xterm buffer，缺失时用 1:1 兜底，不影响浏览器真实值。
      const activeBuffer = terminal.buffer?.active;
      onCursorChangeRef.current({
        row: activeBuffer ? activeBuffer.cursorY + 1 : 1,
        col: activeBuffer ? activeBuffer.cursorX + 1 : 1,
        focused: focusedRef.current,
      });
    });
  };

  const applyFontSize = (terminal: Terminal, fontSize: number) => {
    if (currentFontSizeRef.current === fontSize) {
      return;
    }
    currentFontSizeRef.current = fontSize;
    // xterm 的 cols/rows 属于构造期配置；运行期缩放只更新字体，避免把只读配置一起写回。
    terminal.options = { fontSize };
  };

  const armFocusFromXtermPointer = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target?.closest(".xterm")) {
      return;
    }
    // 只有用户明确点到真实 xterm 区域时，才允许从 viewer 状态重新接管 PTY 尺寸。
    focusActivationArmedRef.current = true;
    suppressPassiveFocusRef.current = false;
  };

  const focusTerminalFromXtermClick = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target?.closest(".xterm")) {
      return;
    }
    terminalRef.current?.focus();
    resizeRef.current?.("focus");
  };

  useEffect(() => {
    if (!props.attached || !hostRef.current || terminalRef.current) {
      return undefined;
    }

    const terminal = new Terminal({
      cursorBlink: true,
      cursorStyle: "block",
      cursorInactiveStyle: "outline",
      // MVP 只需要普通终端渲染；屏幕阅读模式会额外维持可访问性树，增加高输出场景的内存和 CPU 压力。
      screenReaderMode: false,
      scrollback: 2000,
      fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: TERMINAL_FONT_SIZE,
      convertEol: true,
      theme: {
        background: "#08110f",
        foreground: "#d7f7e8",
        cursor: "#d6ff5f",
        selectionBackground: "#285f52",
      },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(hostRef.current);
    const host = hostRef.current;
    const scheduledFrames = new Set<number>();
    const requestTrackedFrame = (callback: () => void) => {
      const frame = window.requestAnimationFrame(() => {
        scheduledFrames.delete(frame);
        callback();
      });
      scheduledFrames.add(frame);
      return frame;
    };
    const dataSubscription = terminal.onData((data) => {
      onInputRef.current(data);
    });
    const cursorMoveSubscription = terminal.onCursorMove(queueCursorReport);
    const writeParsedSubscription = terminal.onWriteParsed(queueCursorReport);
    // 本地 xterm 始终适配当前容器；只有聚焦客户端才把尺寸写回 shared PTY。
    // 未聚焦客户端按 session 的远端 rows/cols 渲染，外层 viewer panel 负责缩放与滚动。
    const resize = (source: ResizeSource = "layout") => {
      const terminalHost = hostRef.current;
      if (!terminalHost) {
        return;
      }
      let proposed = fit.proposeDimensions();
      let hostWidth = terminalHost.clientWidth;
      let hostHeight = terminalHost.clientHeight;
      const remoteSize = sessionSizeRef.current;
      const hostIsRemoteViewerFrame =
        !focusedRef.current &&
        viewerModeRef.current &&
        Boolean(
          remoteSize &&
            proposed &&
            remoteSize.rows === proposed.rows &&
            remoteSize.cols === proposed.cols,
        );
      if (hostIsRemoteViewerFrame && clientSizeRef.current) {
        // viewer 模式下真实 xterm host 会被远端 PTY frame 框住；这时 FitAddon 测到的是
        // 远端画布尺寸，不是浏览器当前可容纳尺寸。继续使用上一轮本地测量值，避免
        // “测到远端尺寸 -> 关闭 viewer -> 又测到本地尺寸 -> 打开 viewer” 的振荡。
        proposed = { rows: clientSizeRef.current.rows, cols: clientSizeRef.current.cols };
        hostWidth = clientSizeRef.current.pixel_width;
        hostHeight = clientSizeRef.current.pixel_height;
      }
      if (proposed) {
        const nextClientSize = {
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostWidth,
          pixel_height: hostHeight,
        };
        clientSizeRef.current = nextClientSize;
        setClientSize((current) =>
          current &&
          current.cols === nextClientSize.cols &&
          current.rows === nextClientSize.rows &&
          current.pixel_width === nextClientSize.pixel_width &&
          current.pixel_height === nextClientSize.pixel_height
            ? current
            : nextClientSize,
        );
      }
      const mismatch = Boolean(
        remoteSize &&
          proposed &&
          (remoteSize.rows !== proposed.rows || remoteSize.cols !== proposed.cols),
      );
      if (focusedRef.current && source !== "focus" && mismatch) {
        // 浏览器窗口 resize 或外层布局变化不是用户主动接管终端；一旦本地可容纳尺寸
        // 和当前 PTY 尺寸不一致，就退回 viewer，避免 focus/blur 与 session_resize 来回振荡。
        focusActivationArmedRef.current = false;
        suppressPassiveFocusRef.current = true;
        focusedRef.current = false;
        setFocused(false);
        const activeElement = document.activeElement;
        if (activeElement instanceof HTMLElement && terminalHost.contains(activeElement)) {
          activeElement.blur();
        }
        queueCursorReport();
      }
      viewerModeRef.current = !focusedRef.current && mismatch;
      if (!focusedRef.current) {
        applyFontSize(terminal, mismatch ? fontSizeForScale(viewerScaleRef.current) : TERMINAL_FONT_SIZE);
        if (remoteSize) {
          if (sameTerminalDimensions(terminal, remoteSize)) {
            queueCursorReport();
            return;
          }
          terminal.resize(remoteSize.cols, remoteSize.rows);
          queueCursorReport();
          return;
        }
      }
      applyFontSize(terminal, TERMINAL_FONT_SIZE);
      // 移动端软键盘或外层 grid 短暂重排时可能把 xterm 容器压到 0 高。
      // 这种尺寸不能写回 shared PTY，否则其他客户端会被同步成一行终端。
      if (proposed && proposed.rows >= MIN_FOCUSED_RESIZE_ROWS && proposed.cols >= MIN_FOCUSED_RESIZE_COLS) {
        viewerModeRef.current = false;
        if (
          sessionSizeRef.current?.rows === proposed.rows &&
          sessionSizeRef.current?.cols === proposed.cols
        ) {
          queueCursorReport();
          return;
        }
        fit.fit();
        onResizeRef.current({
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostWidth,
          pixel_height: hostHeight,
        });
        queueCursorReport();
      }
    };
    resizeRef.current = resize;
    const refreshTerminal = (source: ResizeSource = "layout") => {
      resize(source);
      terminal.refresh(0, Math.max(0, terminal.rows - 1));
    };
    const stabilizeTerminal = (source: ResizeSource = "layout") => {
      // xterm 在 CSS grid / 右侧文件 panel 同步变化时可能先按旧尺寸完成 open/write。
      // 连续两帧刷新可以等浏览器完成布局后再重算 viewport，避免用户必须额外点击才正常显示。
      requestTrackedFrame(() => {
        refreshTerminal(source);
        requestTrackedFrame(() => refreshTerminal(source));
      });
    };
    stabilizeRef.current = stabilizeTerminal;
    const reportFocus = (focused: boolean) => {
      focusedRef.current = focused;
      setFocused(focused);
      queueCursorReport();
    };
    const handleFocusIn = () => {
      if (suppressPassiveFocusRef.current && !focusActivationArmedRef.current) {
        focusedRef.current = false;
        setFocused(false);
        const activeElement = document.activeElement;
        if (activeElement instanceof HTMLElement && host.contains(activeElement)) {
          activeElement.blur();
        }
        queueCursorReport();
        return;
      }
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = false;
      reportFocus(true);
    };
    const handleFocusOut = () => {
      focusActivationArmedRef.current = false;
      reportFocus(false);
    };
    host.addEventListener("focusin", handleFocusIn);
    host.addEventListener("focusout", handleFocusOut);
    terminalRef.current = terminal;
    fitRef.current = fit;
    outputResetVersionRef.current = props.outputResetVersion;
    setWrittenChunkCount(0);
    // attach 输出可能早于 xterm 初始化到达；创建实例时先取走待写队列，避免首屏输出丢失。
    const initialChunks = takeOutputRef.current();
    if (initialChunks.length > 0) {
      terminal.write(initialChunks.join(""), queueCursorReport);
    }
    if (initialChunks.length > 0) {
      setWrittenChunkCount(initialChunks.length);
    }
    queueCursorReport();

    // 初次 attach 只做本地 fit；用户聚焦该终端时才接管 shared PTY 的远端尺寸。
    stabilizeTerminal();
    const handleWindowResize = () => resize("layout");
    window.addEventListener("resize", handleWindowResize);
    const resizeObserver =
      typeof ResizeObserver === "undefined"
        ? undefined
        : new ResizeObserver(() => {
            stabilizeTerminal("layout");
          });
    resizeObserver?.observe(host);
    if (scrollportRef.current) {
      resizeObserver?.observe(scrollportRef.current);
    }

    return () => {
      for (const frame of scheduledFrames) {
        window.cancelAnimationFrame(frame);
      }
      scheduledFrames.clear();
      if (cursorFrameRef.current !== undefined) {
        window.cancelAnimationFrame(cursorFrameRef.current);
        cursorFrameRef.current = undefined;
      }
      window.removeEventListener("resize", handleWindowResize);
      resizeObserver?.disconnect();
      host.removeEventListener("focusin", handleFocusIn);
      host.removeEventListener("focusout", handleFocusOut);
      dataSubscription.dispose();
      cursorMoveSubscription.dispose();
      writeParsedSubscription.dispose();
      terminal.dispose();
      // 清理 host 里的旧 xterm DOM，避免切换 session 后旧终端明文或隐藏 textarea 残留。
      host.replaceChildren();
      terminalRef.current = null;
      fitRef.current = null;
      resizeRef.current = undefined;
      stabilizeRef.current = undefined;
      focusedRef.current = false;
      clientSizeRef.current = undefined;
      viewerModeRef.current = false;
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = false;
      setFocused(false);
      setWrittenChunkCount(0);
    };
  }, [props.attached]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    if (outputResetVersionRef.current !== props.outputResetVersion) {
      outputResetVersionRef.current = props.outputResetVersion;
      // session 切换时 UI 会重置输出队列；同步清屏，避免旧 session 明文留在终端实例中。
      terminal.clear();
      setWrittenChunkCount(0);
    }

    const chunks = takeOutputRef.current();
    if (chunks.length === 0) {
      return;
    }

    terminal.write(chunks.join(""), queueCursorReport);
    setWrittenChunkCount((count) => count + chunks.length);
    stabilizeRef.current?.();
  }, [props.outputResetVersion, props.outputVersion]);

  useEffect(() => {
    if (!props.attached || !props.focusRequest || !terminalRef.current) {
      return undefined;
    }

    // 新建 session 后要直接进入可输入状态；等一帧可以确保 xterm 已完成 open/fit，
    // focusin 事件随后会关闭 viewer 虚线框，并由聚焦客户端上报真实 PTY 尺寸。
    const frame = window.requestAnimationFrame(() => {
      focusActivationArmedRef.current = true;
      suppressPassiveFocusRef.current = false;
      terminalRef.current?.focus();
      stabilizeRef.current?.("focus");
    });
    return () => window.cancelAnimationFrame(frame);
  }, [props.attached, props.focusRequest]);

  return (
    <section
      className={resolutionMismatch ? "terminal-pane terminal-pane-viewer" : "terminal-pane"}
      data-output-chunks={writtenChunkCount}
      data-viewer-mode={resolutionMismatch ? "true" : "false"}
      data-testid="terminal-pane"
    >
      {resolutionMismatch ? (
        <div
          className="terminal-viewer-toolbar"
          aria-label="viewer controls"
          onClick={(event) => event.stopPropagation()}
          onMouseDown={(event) => event.preventDefault()}
        >
          <span className="terminal-viewer-size">{viewerCols && viewerRows ? `${viewerCols}x${viewerRows}` : "viewer"}</span>
          <button
            type="button"
            className="icon-button"
            aria-label="Zoom out"
            title="Zoom out"
            onClick={() => setViewerScale((scale) => clampViewerScale(scale - VIEWER_ZOOM_STEP))}
          >
            <ZoomOut size={15} aria-hidden="true" />
          </button>
          <span className="terminal-viewer-scale">{Math.round(viewerScale * 100)}%</span>
          <button
            type="button"
            className="icon-button"
            aria-label="Zoom in"
            title="Zoom in"
            onClick={() => setViewerScale((scale) => clampViewerScale(scale + VIEWER_ZOOM_STEP))}
          >
            <ZoomIn size={15} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label="Fit"
            title="Fit"
            onClick={fitViewerToScrollport}
          >
            <Maximize2 size={14} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label="Reset zoom"
            title="Reset zoom"
            onClick={() => setViewerScale(1)}
          >
            <RotateCcw size={14} aria-hidden="true" />
          </button>
        </div>
      ) : null}
      <div className="terminal-scrollport" ref={scrollportRef}>
        <div className="terminal-viewer-canvas" ref={canvasRef}>
          <div className="terminal-viewer-frame" ref={frameRef} style={viewerFrameStyle}>
            <div
              className="terminal-host"
              ref={hostRef}
              onMouseDown={armFocusFromXtermPointer}
              onClick={focusTerminalFromXtermClick}
            />
          </div>
        </div>
      </div>
      {!props.attached ? <div className="terminal-placeholder">detached</div> : null}
    </section>
  );
}

function fontSizeForScale(scale: number): number {
  return Math.max(8, Math.round(TERMINAL_FONT_SIZE * clampViewerScale(scale)));
}

function clampViewerScale(scale: number): number {
  return Math.min(VIEWER_MAX_ZOOM, Math.max(VIEWER_MIN_ZOOM, Number(scale.toFixed(2))));
}

function fitScaleForViewer(scrollport: HTMLElement | null, canvas: HTMLElement | null, currentScale: number): number {
  if (!scrollport || !canvas || scrollport.clientWidth <= 0 || scrollport.clientHeight <= 0 || canvas.offsetWidth <= 0 || canvas.offsetHeight <= 0) {
    return 1;
  }
  const widthScale = (scrollport.clientWidth / canvas.offsetWidth) * currentScale;
  const heightScale = (scrollport.clientHeight / canvas.offsetHeight) * currentScale;
  return clampViewerScale(Math.min(widthScale, heightScale));
}
