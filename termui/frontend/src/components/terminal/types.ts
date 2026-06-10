import type { TerminalSize } from "../../protocol/types";

export interface TerminalResyncOptions {
  revealHistory?: boolean;
}

export type TerminalOutputItem =
  | { kind: "data"; bytes: Uint8Array }
  | { kind: "sync"; baseSeq: number }
  | { kind: "snapshot"; bytes: Uint8Array; baseSeq: number; size: TerminalSize; revealHistory?: boolean }
  | { kind: "output"; bytes: Uint8Array; terminalSeq: number }
  | { kind: "resize"; terminalSeq: number; size: TerminalSize }
  | { kind: "exit"; terminalSeq: number };
