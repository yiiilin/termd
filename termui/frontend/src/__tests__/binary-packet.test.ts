import { describe, expect, it } from "vitest";
import {
  decodeBinaryProtocolPacket,
  encodeBinaryProtocolPacket,
} from "../protocol/binary-packet";

describe("binary protobuf protocol packet", () => {
  it("stream_chunk 使用 raw terminal bytes，不再出现 data_base64", () => {
    const terminalBytes = new Uint8Array([0, 114, 97, 119, 45, 116, 101, 114, 109, 255]);
    const packet = {
      version: 3,
      kind: "stream_chunk" as const,
      stream_id: "00000000-0000-0000-0000-000000000321",
      seq: 7,
      payload: {
        type: "session_data" as const,
        session_id: "00000000-0000-0000-0000-000000000301",
        data: terminalBytes,
      },
    };

    const encoded = encodeBinaryProtocolPacket(packet);
    const decoded = decodeBinaryProtocolPacket(encoded);
    const encodedText = new TextDecoder().decode(encoded);

    expect(decoded).toEqual(packet);
    expect(encodedText).not.toContain("data_base64");
    expect(encodedText).not.toContain("AHJhdy10ZXJt");
  });

  it("flow 和 error packet 保留 request/stream 归属字段", () => {
    const flow = {
      version: 3,
      kind: "flow" as const,
      stream_id: "00000000-0000-0000-0000-000000000322",
      ack: 9,
      credit: 4096,
      payload: undefined,
    };
    const error = {
      version: 3,
      kind: "error" as const,
      id: "00000000-0000-0000-0000-000000000323",
      payload: {
        type: "error" as const,
        code: "session_not_found",
        message: "session was not found",
        retryable: false,
      },
    };

    expect(decodeBinaryProtocolPacket(encodeBinaryProtocolPacket(flow))).toEqual(flow);
    expect(decodeBinaryProtocolPacket(encodeBinaryProtocolPacket(error))).toEqual(error);
  });

  it("兼容省略 kind 字段的早期 snapshot terminal frame", () => {
    const legacySnapshotPayload = new Uint8Array([
      0x12, 0x10,
      0x00, 0x00, 0x00, 0x00,
      0x00, 0x00,
      0x00, 0x00,
      0x00, 0x00,
      0x00, 0x00, 0x00, 0x00, 0x03, 0x01,
      0x18, 0x07,
      0x2a, 0x04, 0x08, 0x18, 0x10, 0x50,
      0x32, 0x02, 0x6f, 0x6b,
    ]);
    const packet = new Uint8Array([
      0x08, 0x03,
      0x10, 0x05,
      0xb2, 0x01, legacySnapshotPayload.length,
      ...legacySnapshotPayload,
    ]);

    expect(decodeBinaryProtocolPacket(packet).payload).toEqual({
      type: "terminal_frame",
      frame: {
        kind: "snapshot",
        session_id: "00000000-0000-0000-0000-000000000301",
        base_seq: 7,
        terminal_seq: 0,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        data: new Uint8Array([0x6f, 0x6b]),
      },
    });
  });
});
