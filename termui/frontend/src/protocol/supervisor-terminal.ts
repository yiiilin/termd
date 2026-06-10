import type {
  AttachFramePayload,
  SingleTerminalFramePayload,
  TerminalSize,
  UUID,
} from "./types";
import { base64ToBytes, bytesToBase64, decodeUtf8, encodeUtf8 } from "./wire";

interface SupervisorSnapshotWire {
  size: TerminalSize;
  process_id?: number | null;
  retained_output?: string;
}

type SupervisorTerminalFrameWire =
  | { kind: "snapshot"; base_seq: number; size: TerminalSize; data?: string }
  | { kind: "output"; terminal_seq: number; data?: string }
  | { kind: "resize"; terminal_seq: number; size: TerminalSize }
  | { kind: "exit"; terminal_seq: number; code?: number | null };

export type SupervisorTerminalClientFrame =
  | { type: "input"; data_bytes: Uint8Array }
  | { type: "resize"; size: TerminalSize }
  | { type: "heartbeat_pong"; nonce: string };

export type SupervisorTerminalServerFrame =
  | {
      type: "attach_sync";
      session_id: UUID;
      base_seq: number;
      snapshot: {
        size: TerminalSize;
        process_id?: number | null;
        retained_output_bytes: Uint8Array;
      };
      frames: SingleTerminalFramePayload[];
    }
  | { type: "terminal_frame"; session_id: UUID; frame: SingleTerminalFramePayload }
  | { type: "heartbeat_ping"; nonce: string; timeout_ms: number }
  | { type: "close"; reason: string; message?: string | null };

export function encodeSupervisorTerminalClientFrame(frame: SupervisorTerminalClientFrame): Uint8Array {
  let json: unknown;
  if (frame.type === "input") {
    json = {
      type: "input",
      data: bytesToBase64(frame.data_bytes),
    };
  } else if (frame.type === "resize") {
    json = {
      type: "resize",
      size: frame.size,
    };
  } else {
    json = {
      type: "heartbeat_pong",
      nonce: frame.nonce,
    };
  }
  return encodeLengthPrefixedJson(json);
}

export function decodeSupervisorTerminalClientFrame(bytes: Uint8Array): SupervisorTerminalClientFrame {
  const wire = decodeLengthPrefixedJson(bytes) as { type?: string };
  switch (wire.type) {
    case "input":
      return {
        type: "input",
        data_bytes: base64ToBytes((wire as { data?: string }).data ?? ""),
      };
    case "resize":
      return {
        type: "resize",
        size: (wire as { size: TerminalSize }).size,
      };
    case "heartbeat_pong":
      return {
        type: "heartbeat_pong",
        nonce: (wire as { nonce: string }).nonce,
      };
    default:
      throw new Error("invalid_supervisor_terminal_client_frame");
  }
}

export function decodeSupervisorTerminalServerFrame(bytes: Uint8Array): SupervisorTerminalServerFrame {
  const wire = decodeLengthPrefixedJson(bytes) as { type?: string };
  switch (wire.type) {
    case "attach_sync": {
      const payload = wire as {
        type: "attach_sync";
        session_id: UUID;
        base_seq: number;
        snapshot: SupervisorSnapshotWire;
        frames: SupervisorTerminalFrameWire[];
      };
      return {
        type: "attach_sync",
        session_id: payload.session_id,
        base_seq: payload.base_seq,
        snapshot: {
          size: payload.snapshot.size,
          process_id: payload.snapshot.process_id ?? null,
          retained_output_bytes: base64ToBytes(payload.snapshot.retained_output ?? ""),
        },
        frames: payload.frames.map((frame) => wireTerminalFrameToPayload(payload.session_id, frame)),
      };
    }
    case "terminal_frame": {
      const payload = wire as {
        type: "terminal_frame";
        session_id: UUID;
        frame: SupervisorTerminalFrameWire;
      };
      return {
        type: "terminal_frame",
        session_id: payload.session_id,
        frame: wireTerminalFrameToPayload(payload.session_id, payload.frame),
      };
    }
    case "heartbeat_ping":
      return wire as SupervisorTerminalServerFrame;
    case "close":
      return wire as SupervisorTerminalServerFrame;
    default:
      throw new Error("invalid_supervisor_terminal_frame");
  }
}

export function encodeSupervisorTerminalServerFrame(frame: SupervisorTerminalServerFrame): Uint8Array {
  let json: unknown;
  if (frame.type === "attach_sync") {
    json = {
      type: "attach_sync",
      session_id: frame.session_id,
      base_seq: frame.base_seq,
      snapshot: {
        size: frame.snapshot.size,
        process_id: frame.snapshot.process_id ?? null,
        retained_output: bytesToBase64(frame.snapshot.retained_output_bytes),
      },
      frames: frame.frames.map((item) => terminalFramePayloadToWire(item)),
    };
  } else if (frame.type === "terminal_frame") {
    json = {
      type: "terminal_frame",
      session_id: frame.session_id,
      frame: terminalFramePayloadToWire(frame.frame),
    };
  } else if (frame.type === "heartbeat_ping") {
    json = frame;
  } else {
    json = frame;
  }
  return encodeLengthPrefixedJson(json);
}

export function buildAttachFramePayload(sessionId: UUID, bytes: Uint8Array): AttachFramePayload {
  return {
    session_id: sessionId,
    data_bytes: bytes,
    data_base64: bytesToBase64(bytes),
  };
}

function wireTerminalFrameToPayload(sessionId: UUID, wire: SupervisorTerminalFrameWire): SingleTerminalFramePayload {
  switch (wire.kind) {
    case "snapshot":
      return {
        kind: "snapshot",
        session_id: sessionId,
        base_seq: wire.base_seq,
        size: wire.size,
        data_bytes: base64ToBytes(wire.data ?? ""),
        data_base64: wire.data ?? "",
      };
    case "output":
      return {
        kind: "output",
        session_id: sessionId,
        terminal_seq: wire.terminal_seq,
        data_bytes: base64ToBytes(wire.data ?? ""),
        data_base64: wire.data ?? "",
      };
    case "resize":
      return {
        kind: "resize",
        session_id: sessionId,
        terminal_seq: wire.terminal_seq,
        size: wire.size,
      };
    case "exit":
      return {
        kind: "exit",
        session_id: sessionId,
        terminal_seq: wire.terminal_seq,
        code: wire.code ?? null,
      };
    default:
      throw new Error("invalid_supervisor_terminal_frame");
  }
}

function terminalFramePayloadToWire(frame: SingleTerminalFramePayload): SupervisorTerminalFrameWire {
  switch (frame.kind) {
    case "snapshot":
      return {
        kind: "snapshot",
        base_seq: frame.base_seq,
        size: frame.size,
        data: bytesToBase64(frame.data_bytes ?? base64ToBytes(frame.data_base64 ?? "")),
      };
    case "output":
      return {
        kind: "output",
        terminal_seq: frame.terminal_seq,
        data: bytesToBase64(frame.data_bytes ?? base64ToBytes(frame.data_base64 ?? "")),
      };
    case "resize":
      return {
        kind: "resize",
        terminal_seq: frame.terminal_seq,
        size: frame.size,
      };
    case "exit":
      return {
        kind: "exit",
        terminal_seq: frame.terminal_seq,
        code: frame.code ?? null,
      };
    default:
      throw new Error("invalid_supervisor_terminal_frame");
  }
}

function encodeLengthPrefixedJson(value: unknown): Uint8Array {
  const payload = encodeUtf8(JSON.stringify(value));
  const frame = new Uint8Array(4 + payload.byteLength);
  const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
  view.setUint32(0, payload.byteLength, true);
  frame.set(payload, 4);
  return frame;
}

function decodeLengthPrefixedJson(bytes: Uint8Array): unknown {
  if (bytes.byteLength < 4) {
    throw new Error("invalid_supervisor_terminal_frame");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const payloadLength = view.getUint32(0, true);
  if (payloadLength !== bytes.byteLength - 4) {
    throw new Error("invalid_supervisor_terminal_frame");
  }
  return JSON.parse(decodeUtf8(bytes.subarray(4)));
}
