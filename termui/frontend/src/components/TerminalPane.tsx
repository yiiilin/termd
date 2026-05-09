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
const VIEWER_ZOOM_STEP = 0.1;
const VIEWER_MIN_ZOOM = 0.5;
const VIEWER_MAX_ZOOM = 1.4;

interface TerminalPaneProps {
  chunks: string[];
  attached: boolean;
  sessionSize?: TerminalSize;
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
  const writtenChunksRef = useRef(0);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);
  const onCursorChangeRef = useRef(props.onCursorChange);
  const sessionSizeRef = useRef(props.sessionSize);
  const viewerScaleRef = useRef(1);
  const resizeRef = useRef<(() => void) | undefined>(undefined);
  const cursorFrameRef = useRef<number | undefined>(undefined);
  const focusedRef = useRef(false);
  const currentFontSizeRef = useRef(TERMINAL_FONT_SIZE);
  const [focused, setFocused] = useState(false);
  const [viewerScale, setViewerScale] = useState(1);
  const fitViewerToScrollport = () => setViewerScale(fitScaleForViewer(scrollportRef.current, frameRef.current, viewerScaleRef.current));
  const viewerMode = props.attached && !focused;
  const viewerCols = props.sessionSize?.cols ?? 0;
  const viewerRows = props.sessionSize?.rows ?? 0;
  const viewerPixelWidth = props.sessionSize?.pixel_width ?? 0;
  const viewerPixelHeight = props.sessionSize?.pixel_height ?? 0;
  const viewerFontSize = fontSizeForScale(viewerScale);
  const viewerFrameStyle =
    viewerMode && viewerCols > 0 && viewerRows > 0
      ? {
          // 优先使用聚焦端上报的像素尺寸；缺失时才按 rows/cols 估算 PTY 画布。
          width:
            viewerPixelWidth > 0
              ? `${Math.ceil(viewerPixelWidth * viewerScale) + TERMINAL_FRAME_CHROME_PX}px`
              : `calc(${viewerCols}ch + ${TERMINAL_FRAME_CHROME_PX}px)`,
          height:
            viewerPixelHeight > 0
              ? `${Math.ceil(viewerPixelHeight * viewerScale) + TERMINAL_FRAME_CHROME_PX}px`
              : `${Math.ceil(viewerRows * viewerFontSize * TERMINAL_LINE_HEIGHT) + TERMINAL_FRAME_CHROME_PX}px`,
          fontSize: `${viewerFontSize}px`,
          fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
        }
      : undefined;

  useEffect(() => {
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onCursorChangeRef.current = props.onCursorChange;
    sessionSizeRef.current = props.sessionSize;
  }, [props.onCursorChange, props.onInput, props.onResize, props.sessionSize]);

  useEffect(() => {
    viewerScaleRef.current = viewerScale;
    if (!focusedRef.current) {
      resizeRef.current?.();
    }
  }, [viewerScale]);

  useEffect(() => {
    resizeRef.current?.();
  }, [focused]);

  useEffect(() => {
    sessionSizeRef.current = props.sessionSize;
    if (!focusedRef.current) {
      resizeRef.current?.();
    }
  }, [props.sessionSize?.cols, props.sessionSize?.rows]);

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

  const focusTerminalFromXtermClick = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target?.closest(".xterm")) {
      return;
    }
    terminalRef.current?.focus();
  };

  useEffect(() => {
    if (!props.attached || !hostRef.current || terminalRef.current) {
      return undefined;
    }

    const terminal = new Terminal({
      cursorBlink: true,
      cursorStyle: "block",
      cursorInactiveStyle: "outline",
      screenReaderMode: true,
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
    const dataSubscription = terminal.onData((data) => {
      onInputRef.current(data);
    });
    const cursorMoveSubscription = terminal.onCursorMove(queueCursorReport);
    const writeParsedSubscription = terminal.onWriteParsed(queueCursorReport);
    // 本地 xterm 始终适配当前容器；只有聚焦客户端才把尺寸写回 shared PTY。
    // 未聚焦客户端按 session 的远端 rows/cols 渲染，外层 viewer panel 负责缩放与滚动。
    const resize = () => {
      const terminalHost = hostRef.current;
      if (!terminalHost) {
        return;
      }
      if (!focusedRef.current) {
        const remoteSize = sessionSizeRef.current;
        applyFontSize(terminal, fontSizeForScale(viewerScaleRef.current));
        if (remoteSize) {
          terminal.resize(remoteSize.cols, remoteSize.rows);
          queueCursorReport();
          return;
        }
      }
      applyFontSize(terminal, TERMINAL_FONT_SIZE);
      fit.fit();
      const proposed = fit.proposeDimensions();
      if (proposed) {
        onResizeRef.current({
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostRef.current?.clientWidth ?? 0,
          pixel_height: hostRef.current?.clientHeight ?? 0,
        });
        queueCursorReport();
      }
    };
    resizeRef.current = resize;
    const reportFocus = (focused: boolean) => {
      focusedRef.current = focused;
      setFocused(focused);
      queueCursorReport();
    };
    const handleFocusIn = () => reportFocus(true);
    const handleFocusOut = () => reportFocus(false);
    host.addEventListener("focusin", handleFocusIn);
    host.addEventListener("focusout", handleFocusOut);
    terminalRef.current = terminal;
    fitRef.current = fit;
    writtenChunksRef.current = 0;
    // attach 输出可能早于 xterm 初始化到达；创建实例时补写已有 chunks，避免首屏输出丢失。
    for (const chunk of props.chunks) {
      terminal.write(chunk, queueCursorReport);
    }
    writtenChunksRef.current = props.chunks.length;
    queueCursorReport();

    // 初次 attach 只做本地 fit；用户聚焦该终端时才接管 shared PTY 的远端尺寸。
    const frame = window.requestAnimationFrame(resize);
    window.addEventListener("resize", resize);

    return () => {
      window.cancelAnimationFrame(frame);
      if (cursorFrameRef.current !== undefined) {
        window.cancelAnimationFrame(cursorFrameRef.current);
        cursorFrameRef.current = undefined;
      }
      window.removeEventListener("resize", resize);
      host.removeEventListener("focusin", handleFocusIn);
      host.removeEventListener("focusout", handleFocusOut);
      dataSubscription.dispose();
      cursorMoveSubscription.dispose();
      writeParsedSubscription.dispose();
      terminal.dispose();
      terminalRef.current = null;
      fitRef.current = null;
      resizeRef.current = undefined;
      focusedRef.current = false;
      setFocused(false);
    };
  }, [props.attached]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      writtenChunksRef.current = props.chunks.length;
      return;
    }
    if (props.chunks.length < writtenChunksRef.current) {
      // session 切换时 UI 会清空 chunks；同步清屏，避免旧 session 明文留在终端实例中。
      terminal.clear();
      writtenChunksRef.current = 0;
    }
    for (let index = writtenChunksRef.current; index < props.chunks.length; index += 1) {
      terminal.write(props.chunks[index], queueCursorReport);
    }
    writtenChunksRef.current = props.chunks.length;
  }, [props.chunks]);

  return (
    <section
      className={viewerMode ? "terminal-pane terminal-pane-viewer" : "terminal-pane"}
      data-output-chunks={props.chunks.length}
      data-viewer-mode={viewerMode ? "true" : "false"}
      data-testid="terminal-pane"
    >
      {viewerMode ? (
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
            <div className="terminal-host" ref={hostRef} onClick={focusTerminalFromXtermClick} />
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
