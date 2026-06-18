import { afterEach, describe, expect, it, vi } from "vitest";

const openWebSocketMock = vi.hoisted(() => vi.fn());
const sendOuterMessageMock = vi.hoisted(() => vi.fn());
const verifyEd25519SignatureMock = vi.hoisted(() => vi.fn(async () => true));
const generateE2eeKeyPairMock = vi.hoisted(() => vi.fn(() => ({ publicKeyWire: "device-e2ee-public-key" })));
const e2eeSessionDeviceMock = vi.hoisted(() => vi.fn(() => ({ kind: "device-e2ee-session" })));

vi.mock("../protocol/socket-transport", async () => {
  const actual = await vi.importActual<typeof import("../protocol/socket-transport")>("../protocol/socket-transport");
  return {
    ...actual,
    openWebSocket: openWebSocketMock,
    sendOuterMessage: sendOuterMessageMock,
  };
});

vi.mock("../protocol/auth", () => ({
  daemonE2eeSigningInputBytes: () => new Uint8Array([1, 2, 3]),
  decodeEd25519PublicKey: () => new Uint8Array([4, 5, 6]),
  e2eeAuthTranscriptDigestWire: () => "mock-transcript-digest",
  verifyEd25519Signature: verifyEd25519SignatureMock,
}));

vi.mock("../protocol/e2ee", () => ({
  E2eeSession: {
    device: e2eeSessionDeviceMock,
  },
  generateE2eeKeyPair: generateE2eeKeyPairMock,
}));

import { performDirectHandshake } from "../protocol/direct-handshake";
import type { TermdDiagnosticEvent } from "../diagnostics";
import type { QueuedMessage } from "../protocol/socket-transport";
import { PROTOCOL_PACKET_VERSION } from "../protocol/types";

const SERVER_ID = "00000000-0000-0000-0000-000000000101";
const DEVICE_ID = "00000000-0000-0000-0000-000000000201";
const SOCKET_OPEN = globalThis.WebSocket?.OPEN ?? 1;
const SOCKET_CLOSED = globalThis.WebSocket?.CLOSED ?? 3;

function testDiagnostics(): { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] } {
  return globalThis as { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] };
}

afterEach(() => {
  vi.clearAllMocks();
  delete testDiagnostics().__TERMD_TRACE__;
  delete testDiagnostics().__TERMD_DIAG_EVENTS__;
});

describe("performDirectHandshake", () => {
  it("客户端发出自己的 e2ee_key_exchange 前 socket 已关闭时，connect 失败而不是返回 dead client", async () => {
    const socketState: { readyState: number } = { readyState: SOCKET_OPEN };
    const socket = {
      get readyState() {
        return socketState.readyState;
      },
      close: vi.fn(() => {
        socketState.readyState = SOCKET_CLOSED;
      }),
    } as unknown as WebSocket;
    const inbox = {
      read: vi
        .fn()
        .mockResolvedValueOnce({
          envelope: {
            type: "route_ready",
            payload: { server_id: SERVER_ID, role: "client" },
          },
        })
        .mockResolvedValueOnce({
          envelope: {
            type: "hello",
            payload: { server_id: SERVER_ID },
          },
        })
        .mockImplementationOnce(async () => {
          socketState.readyState = SOCKET_CLOSED;
          return {
            envelope: {
              type: "e2ee_key_exchange",
              payload: {
                server_id: SERVER_ID,
                public_key: "daemon-e2ee-public-key",
                packet_version: PROTOCOL_PACKET_VERSION,
                signature: "mock-signature",
              },
            },
          };
        }),
      rejectPending: vi.fn(),
    };
    openWebSocketMock.mockResolvedValue(socket);

    await expect(
      performDirectHandshake("ws://127.0.0.1:8765/ws", SERVER_ID, DEVICE_ID, {
        timeoutMs: 3000,
        socketOpenTimeoutMs: 3000,
        expectedDaemonPublicKey: "daemon-public-key",
        createInbox: () => inbox,
      }),
    ).rejects.toMatchObject({
      code: "connection_closed",
    });

    expect(sendOuterMessageMock).toHaveBeenCalledTimes(1);
    expect(inbox.rejectPending).toHaveBeenCalledTimes(1);
    expect(generateE2eeKeyPairMock).toHaveBeenCalledTimes(1);
    expect(verifyEd25519SignatureMock).toHaveBeenCalledTimes(1);
  });

  it("握手 timeout 会记录结构化诊断事件", async () => {
    testDiagnostics().__TERMD_TRACE__ = true;
    testDiagnostics().__TERMD_DIAG_EVENTS__ = [];

    const socket = {
      readyState: SOCKET_OPEN,
      close: vi.fn(),
    } as unknown as WebSocket;
    const inbox = {
      read: vi.fn(async (): Promise<QueuedMessage> => new Promise(() => undefined)),
      rejectPending: vi.fn(),
    };
    openWebSocketMock.mockResolvedValue(socket);

    await expect(
      performDirectHandshake("ws://127.0.0.1:8765/ws", SERVER_ID, DEVICE_ID, {
        timeoutMs: 5,
        socketOpenTimeoutMs: 5,
        expectedDaemonPublicKey: "daemon-public-key",
        createInbox: () => inbox,
      }),
    ).rejects.toMatchObject({
      code: "route_prelude_timeout",
    });

    expect(testDiagnostics().__TERMD_DIAG_EVENTS__).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          name: "protocol_timeout",
          fields: expect.objectContaining({
            layer: "client",
            phase: "route_prelude",
            timeout_code: "route_prelude_timeout",
            timeout_ms: 5,
            server_id: SERVER_ID,
            device_id: DEVICE_ID,
          }),
        }),
      ]),
    );
  });
});
