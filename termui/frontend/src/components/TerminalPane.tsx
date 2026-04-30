import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import type { AttachRole, TerminalSize } from "../protocol/types";

interface TerminalPaneProps {
  chunks: string[];
  attached: boolean;
  role?: AttachRole;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
}

export function TerminalPane(props: TerminalPaneProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const writtenChunksRef = useRef(0);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);

  useEffect(() => {
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
  }, [props.onInput, props.onResize]);

  useEffect(() => {
    if (!props.attached || !hostRef.current || terminalRef.current) {
      return undefined;
    }

    const terminal = new Terminal({
      cursorBlink: true,
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
    terminal.onData((data) => onInputRef.current(data));
    terminalRef.current = terminal;
    fitRef.current = fit;
    writtenChunksRef.current = 0;
    // attach 输出可能早于 xterm 初始化到达；创建实例时补写已有 chunks，避免首屏输出丢失。
    for (const chunk of props.chunks) {
      terminal.write(chunk);
    }
    writtenChunksRef.current = props.chunks.length;

    // xterm 的 cols/rows 是 terminal attach 的协议边界，UI 只上报尺寸，不决定业务权限。
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
      }
    };
    const frame = window.requestAnimationFrame(resize);
    window.addEventListener("resize", resize);

    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", resize);
      terminal.dispose();
      terminalRef.current = null;
      fitRef.current = null;
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
      terminal.write(props.chunks[index]);
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
      <div className="terminal-role" aria-live="polite">
        {props.role ?? "detached"}
      </div>
    </section>
  );
}
