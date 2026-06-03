import type { TerminalSize } from "../../protocol/types";

export type TerminalOutputItem =
  | { kind: "data"; bytes: Uint8Array }
  | { kind: "snapshot"; bytes: Uint8Array; baseSeq: number; size: TerminalSize }
  | { kind: "output"; bytes: Uint8Array; terminalSeq: number }
  | { kind: "resize"; terminalSeq: number; size: TerminalSize }
  | { kind: "exit"; terminalSeq: number };
