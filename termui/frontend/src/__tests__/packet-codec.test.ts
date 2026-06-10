import { describe, expect, it } from "vitest";
import { PROTOCOL_PACKET_VERSION } from "../protocol/types";
import type { ProtocolPacket } from "../protocol/types";
import {
  binaryPacketToProtocol,
  protocolPacketToBinary,
} from "../protocol/packet-codec";
import {
  envelopeTypeForProtocolEventMethod,
  legacyEnvelopeTypeForProtocolMethod,
  protocolEventMethodForLegacyEnvelopeType,
  protocolMethodNeedsEmptyAck,
} from "../protocol/methods";

const REQUEST_ID = "00000000-0000-0000-0000-000000000701";
const STREAM_ID = "00000000-0000-0000-0000-000000000702";
const SESSION_ID = "00000000-0000-0000-0000-000000000703";

function codecRoundTrip(packet: ProtocolPacket): ProtocolPacket {
  return binaryPacketToProtocol(protocolPacketToBinary(packet));
}

describe("protocol packet codec", () => {
  it("request、response、event 和 stream_open JSON payload 可以往返", () => {
    const packets: ProtocolPacket[] = [
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "request",
        id: REQUEST_ID,
        method: "session.list",
        payload: {},
      },
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "response",
        id: REQUEST_ID,
        method: "session.list",
        payload: { sessions: [] },
      },
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "event",
        method: "session.resized",
        payload: {
          session_id: SESSION_ID,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      },
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "stream_open",
        id: REQUEST_ID,
        stream_id: STREAM_ID,
        method: "terminal.attach",
        payload: { session_id: SESSION_ID, watch_updates: true },
      },
    ];

    for (const packet of packets) {
      expect(codecRoundTrip(packet)).toEqual(packet);
    }
  });

  it("stream_chunk 的 terminal session data 使用 raw bytes 往返", () => {
    const dataBytes = new Uint8Array([0, 115, 116, 114, 101, 97, 109, 255]);
    const packet: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 3,
      payload: {
        session_id: SESSION_ID,
        data_bytes: dataBytes,
      },
    };

    const binary = protocolPacketToBinary(packet);
    const roundTripped = binaryPacketToProtocol(binary);

    expect(binary.version).toBe(PROTOCOL_PACKET_VERSION);
    expect(binary.payload).toEqual({
      type: "session_data",
      session_id: SESSION_ID,
      data: dataBytes,
    });
    expect(roundTripped).toMatchObject({
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 3,
      payload: {
        session_id: SESSION_ID,
        data_base64: "AHN0cmVhbf8=",
      },
    });
    expect((roundTripped.payload as { data_bytes?: Uint8Array }).data_bytes).toEqual(dataBytes);
  });

  it("stream_chunk 的 file chunk 和 stream_end 可以往返", () => {
    const fileBytes = new Uint8Array([102, 105, 108, 101]);
    const fileChunk: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 4,
      payload: {
        session_id: SESSION_ID,
        offset_bytes: 8,
        data_bytes: fileBytes,
        size_bytes: 12,
        eof: false,
      },
    };
    const streamEnd: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_end",
      stream_id: STREAM_ID,
      seq: 5,
      payload: {},
    };

    const roundTrippedChunk = codecRoundTrip(fileChunk);

    expect(protocolPacketToBinary(fileChunk).payload).toEqual({
      type: "file_chunk",
      session_id: SESSION_ID,
      offset_bytes: 8,
      data: fileBytes,
      size_bytes: 12,
      eof: false,
    });
    expect(roundTrippedChunk).toMatchObject({
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 4,
      payload: {
        session_id: SESSION_ID,
        offset_bytes: 8,
        size_bytes: 12,
        eof: false,
      },
    });
    expect((roundTrippedChunk.payload as { data_bytes?: Uint8Array }).data_bytes).toEqual(fileBytes);
    expect(codecRoundTrip(streamEnd)).toEqual(streamEnd);
  });

  it("terminal frame payload 使用专用二进制 payload 往返", () => {
    const frameBytes = new Uint8Array([116, 101, 114, 109]);
    const packet: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 6,
      payload: {
        kind: "output",
        session_id: SESSION_ID,
        terminal_seq: 42,
        data_bytes: frameBytes,
      },
    };

    const binary = protocolPacketToBinary(packet);
    const roundTripped = binaryPacketToProtocol(binary);

    expect(binary.payload).toEqual({
      type: "terminal_frame",
      frame: {
        kind: "output",
        session_id: SESSION_ID,
        terminal_seq: 42,
        data: frameBytes,
      },
    });
    expect(roundTripped).toEqual({
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: STREAM_ID,
      seq: 6,
      payload: {
        kind: "output",
        session_id: SESSION_ID,
        terminal_seq: 42,
        data_bytes: frameBytes,
      },
    });
  });
});

describe("protocol method registry", () => {
  it("覆盖 TS 侧 method 到 legacy envelope 的映射", () => {
    expect(legacyEnvelopeTypeForProtocolMethod("pair.request")).toBe("pair_request");
    expect(legacyEnvelopeTypeForProtocolMethod("auth.verify")).toBe("auth");
    expect(legacyEnvelopeTypeForProtocolMethod("session.create")).toBe("session_create");
    expect(legacyEnvelopeTypeForProtocolMethod("terminal.create")).toBe("session_create");
    expect(legacyEnvelopeTypeForProtocolMethod("terminal.attach")).toBe("session_attach");
    expect(legacyEnvelopeTypeForProtocolMethod("session.data")).toBe("session_data");
    expect(legacyEnvelopeTypeForProtocolMethod("session.file_download_chunk")).toBe("session_file_download_chunk");
    expect(legacyEnvelopeTypeForProtocolMethod("unknown.method")).toBeUndefined();
  });

  it("覆盖 TS 侧 event method 到 envelope 的映射，并保留 mock daemon 反向映射", () => {
    expect(envelopeTypeForProtocolEventMethod("auth.challenge")).toBe("auth_challenge");
    expect(envelopeTypeForProtocolEventMethod("session.files")).toBe("session_files_result");
    expect(envelopeTypeForProtocolEventMethod("session.cwd")).toBe("session_cwd_changed");
    expect(envelopeTypeForProtocolEventMethod("session.git")).toBe("session_git_result");
    expect(envelopeTypeForProtocolEventMethod("session.resized")).toBe("session_resized");
    expect(envelopeTypeForProtocolEventMethod("unknown.event")).toBeUndefined();
    expect(protocolEventMethodForLegacyEnvelopeType("session_files_result")).toBe("session.files");
    expect(protocolEventMethodForLegacyEnvelopeType("session_git_result")).toBe("session.git");
    expect(protocolEventMethodForLegacyEnvelopeType("session_renamed")).toBe("session.renamed");
  });

  it("registry 标记 legacy 兼容路径需要的空 ack 方法", () => {
    expect(protocolMethodNeedsEmptyAck("auth")).toBe(true);
    expect(protocolMethodNeedsEmptyAck("auth.verify")).toBe(true);
    expect(protocolMethodNeedsEmptyAck("client.hello")).toBe(true);
    expect(protocolMethodNeedsEmptyAck("session.cursor")).toBe(true);
    expect(protocolMethodNeedsEmptyAck("session.list")).toBe(false);
  });
});
