import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import type { SessionCursorPresence, TerminalSize } from "../protocol/types";

interface TerminalPaneProps {
  chunks: string[];
  attached: boolean;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
  onCursorChange?: (presence: SessionCursorPresence) => void;
}

export function TerminalPane(props: TerminalPaneProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const writtenChunksRef = useRef(0);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);
  const onCursorChangeRef = useRef(props.onCursorChange);
  const cursorFrameRef = useRef<number | undefined>(undefined);
  const focusedRef = useRef(false);

  useEffect(() => {
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onCursorChangeRef.current = props.onCursorChange;
  }, [props.onCursorChange, props.onInput, props.onResize]);

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
      fontSize: 13,
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
    const reportFocus = (focused: boolean) => {
      focusedRef.current = focused;
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

    // xterm 的 cols/rows 是 terminal attach 的协议边界，UI 只上报尺寸，不决定业务控制权。
    const resize = () => {
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
      focusedRef.current = false;
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
      className="terminal-pane"
      data-output-chunks={props.chunks.length}
      data-testid="terminal-pane"
      onClick={() => terminalRef.current?.focus()}
    >
      <div className="terminal-host" ref={hostRef} />
      {!props.attached ? <div className="terminal-placeholder">detached</div> : null}
    </section>
  );
}
