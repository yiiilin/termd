import { describe, expect, it } from "vitest";
import {
  type AttachRole,
  type DaemonClientSummaryPayload,
  type DaemonStatusResultPayload,
  type Envelope,
  type PairingQrPayload,
  type RelayAdmissionPayload,
  type RouteHelloPayload,
  type RouteReadyPayload,
  type RouteRole,
  type SessionActivityPayload,
  type SessionFileDownloadChunkPayload,
  type SessionFileDownloadChunkResultPayload,
  type SessionState,
  type TerminalFramePayload,
} from "../protocol/types";
import {
  PROTOCOL_PACKET_VERSION,
  type ProtocolPacket,
  type SessionCursorPayload,
} from "../test/legacy-protocol-stubs";
import { parsePairingQrPayload } from "../protocol/pairing-payload";
import { envelope } from "../protocol/wire";

describe("协议类型", () => {
  it("统一 envelope 只暴露 type 和 payload 字段", () => {
    const message = envelope("session_resize", {
      session_id: "00000000-0000-0000-0000-000000000001",
      size: { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 },
    });

    expect(Object.keys(message)).toEqual(["type", "payload"]);
    expect(message.type).toBe("session_resize");
    expect(message.payload.size.rows).toBe(40);
  });

  it("状态枚举只表达个人 shared-control 语义", () => {
    const states: SessionState[] = ["created", "running", "closed"];
    const roles: AttachRole[] = ["operator"];
    const routeRoles: RouteRole[] = ["client", "daemon_control", "daemon_data", "daemon_mux"];
    const forbidden = ["admin", "owner", "member", "tenant"];

    expect(states).toEqual(["created", "running", "closed"]);
    expect(roles).toEqual(["operator"]);
    expect(routeRoles).toEqual(["client", "daemon_control", "daemon_data", "daemon_mux"]);
    expect(JSON.stringify({ states, roles }).toLowerCase()).not.toContain(forbidden.join("|"));
  });

  it("route prelude payload 与 Rust proto 公开路由字段对齐", () => {
    const routeHello: RouteHelloPayload = {
      server_id: "00000000-0000-0000-0000-000000000001",
      role: "daemon_data",
      protocol_version: PROTOCOL_PACKET_VERSION,
      nonce: "route-nonce",
      route_generation: "route-generation",
      client_id: 7,
      data_token: "data-token",
      timestamp_ms: 1_710_000_000_000,
    };
    const routeReady: RouteReadyPayload = {
      server_id: routeHello.server_id,
      role: "daemon_data",
    };
    const daemonAdmission: RelayAdmissionPayload = {
      kind: "daemon",
      token: "relay-daemon-admission-token",
    };

    expect(envelope("route_hello", routeHello)).toEqual({
      type: "route_hello",
      payload: routeHello,
    });
    expect(envelope("route_ready", routeReady)).toEqual({
      type: "route_ready",
      payload: routeReady,
    });
    expect(JSON.parse(JSON.stringify(routeHello))).toMatchObject({
      role: "daemon_data",
      client_id: 7,
      data_token: "data-token",
    });
    expect(daemonAdmission).toEqual({
      kind: "daemon",
      token: "relay-daemon-admission-token",
    });
  });

  it("TypeScript wire shape 和 JSON envelope 可序列化", () => {
    const message: Envelope = envelope("session_list", {});
    const raw = JSON.stringify(message);

    expect(raw).toBe('{"type":"session_list","payload":{}}');
  });

  it("0.2 packet 支持 request/response/stream/error 的稳定 JSON 形状", () => {
    const requestId = "00000000-0000-0000-0000-000000000010";
    const streamId = "00000000-0000-0000-0000-000000000020";
    const request: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "request",
      id: requestId,
      method: "session.list",
      payload: {},
    };
    const chunk: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: streamId,
      seq: 7,
      payload: { data_base64: "YWJj" },
    };
    const flow: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "flow",
      stream_id: streamId,
      ack: 7,
      credit: 64,
      payload: {},
    };
    const error: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "error",
      id: requestId,
      payload: { code: "timeout", message: "operation timed out", retryable: true },
    };

    expect(JSON.parse(JSON.stringify(request))).toMatchObject({
      version: 3,
      kind: "request",
      id: requestId,
      method: "session.list",
    });
    expect(JSON.parse(JSON.stringify(chunk))).toMatchObject({
      kind: "stream_chunk",
      stream_id: streamId,
      seq: 7,
    });
    expect(JSON.parse(JSON.stringify(flow))).toMatchObject({
      kind: "flow",
      ack: 7,
      credit: 64,
    });
    expect(JSON.parse(JSON.stringify(error))).toMatchObject({
      kind: "error",
      id: requestId,
      payload: { retryable: true },
    });
  });

  it("terminal frame 区分 snapshot 和 session 级 tail 序号", () => {
    const sessionId = "00000000-0000-0000-0000-000000000001";
    const size = { rows: 32, cols: 120, pixel_width: 0, pixel_height: 0 };
    const snapshot: TerminalFramePayload = {
      kind: "snapshot",
      session_id: sessionId,
      base_seq: 1024,
      size,
      data_base64: "c25hcHNob3Q=",
    };
    const output: TerminalFramePayload = {
      kind: "output",
      session_id: sessionId,
      terminal_seq: 1025,
      data_base64: "b3V0cHV0",
    };
    const resize: TerminalFramePayload = {
      kind: "resize",
      session_id: sessionId,
      terminal_seq: 1026,
      size,
    };
    const batch: TerminalFramePayload = {
      kind: "batch",
      session_id: sessionId,
      frames: [output, resize],
    };

    expect(envelope("terminal_frame", snapshot)).toEqual({
      type: "terminal_frame",
      payload: snapshot,
    });
    expect(JSON.parse(JSON.stringify(output))).toMatchObject({
      kind: "output",
      terminal_seq: 1025,
    });
    expect(JSON.parse(JSON.stringify(resize))).toMatchObject({
      kind: "resize",
      terminal_seq: 1026,
      size,
    });
    expect(JSON.parse(JSON.stringify(batch))).toMatchObject({
      kind: "batch",
      frames: [
        { kind: "output", terminal_seq: 1025 },
        { kind: "resize", terminal_seq: 1026 },
      ],
    });
  });

  it("光标状态只同步位置和 xterm 聚焦状态", () => {
    const cursor: SessionCursorPayload = {
      session_id: "00000000-0000-0000-0000-000000000001",
      row: 12,
      col: 8,
      focused: true,
    };
    const client: DaemonClientSummaryPayload = {
      client_id: "00000000-0000-0000-0000-000000000002",
      device_id: "00000000-0000-0000-0000-000000000003",
      peer_ip: "192.0.2.10",
      online: true,
      connected_at_ms: 1,
      last_seen_at_ms: 2,
      attached_session_ids: [cursor.session_id],
      cursor_session_id: cursor.session_id,
      cursor_row: cursor.row,
      cursor_col: cursor.col,
      cursor_focused: cursor.focused,
    };

    expect(client).toMatchObject({
      cursor_row: 12,
      cursor_col: 8,
      cursor_focused: true,
    });
    expect("selection_start_row" in client).toBe(false);
  });

  it("后台活动和分块下载 payload 不携带明文路径外的权限材料", () => {
    const sessionId = "00000000-0000-0000-0000-000000000001";
    const activity: SessionActivityPayload = {
      session_id: sessionId,
      timestamp_ms: 1_710_000_000_000,
    };
    const chunkRequest: SessionFileDownloadChunkPayload = {
      session_id: sessionId,
      path: "/home/me/large.log",
      offset_bytes: 262_144,
      max_bytes: 262_144,
    };
    const chunkResult: SessionFileDownloadChunkResultPayload = {
      session_id: sessionId,
      path: "/home/me/large.log",
      offset_bytes: 262_144,
      data_base64: "Y2h1bms=",
      next_offset_bytes: 262_149,
      size_bytes: 1_000_000,
      eof: false,
      modified_at_ms: null,
    };

    expect(envelope("session_activity", activity)).toEqual({
      type: "session_activity",
      payload: activity,
    });
    expect(envelope("session_file_download_chunk", chunkRequest).payload.max_bytes).toBe(262_144);
    expect(envelope("session_file_download_chunk_result", chunkResult).payload).toMatchObject({
      data_base64: "Y2h1bms=",
      eof: false,
    });
  });

  it("daemon 状态 payload 只包含轻量只读服务器指标", () => {
    const status: DaemonStatusResultPayload = {
      host_name: "devbox",
      load_avg: [0.1, 0.2, 0.3],
      uptime_seconds: 3600,
      cpu_percent: 12.5,
      memory_total_bytes: 8 * 1024 * 1024,
      memory_available_bytes: 4 * 1024 * 1024,
      disk_total_bytes: 100 * 1024 * 1024,
      disk_available_bytes: 40 * 1024 * 1024,
      network_rx_bytes: 12 * 1024 * 1024,
      network_tx_bytes: 3 * 1024 * 1024,
      process_count: 42,
      atop_available: false,
    };

    expect(envelope("daemon_status", {})).toEqual({
      type: "daemon_status",
      payload: {},
    });
    expect(envelope("daemon_status_result", status).payload).toMatchObject({
      host_name: "devbox",
      load_avg: [0.1, 0.2, 0.3],
      network_rx_bytes: 12 * 1024 * 1024,
      network_tx_bytes: 3 * 1024 * 1024,
      atop_available: false,
    });
    expect(JSON.stringify(status)).not.toContain("session_data");
  });

  it("QR pairing payload 识别 v1 trust anchor 和 v2 trusted relay 邀请", () => {
    const payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 1,
      token: "pair-token",
      server_id: "00000000-0000-0000-0000-000000000001",
      daemon_public_key: "ed25519-v1:daemon-public",
      expires_at_ms: 1710000060000,
    };

    expect(parsePairingQrPayload(JSON.stringify(payload))).toEqual(payload);
    const v2Payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 2,
      token: "pair-token-v2",
      server_id: "00000000-0000-0000-0000-000000000001",
      expires_at_ms: 1710000060000,
    };
    expect(parsePairingQrPayload(JSON.stringify(v2Payload))).toEqual(v2Payload);
    expect(
      parsePairingQrPayload(
        JSON.stringify({
          ...payload,
          ws_url: "wss://relay.example/ws/00000000-0000-0000-0000-000000000001/client",
        }),
      ),
    ).toMatchObject({
      ...payload,
      ws_url: "wss://relay.example/ws/00000000-0000-0000-0000-000000000001/client",
    });
    expect(parsePairingQrPayload("plain-token")).toBeUndefined();
    expect(
      parsePairingQrPayload(
        JSON.stringify({
          ...payload,
          daemon_public_key: undefined,
        }),
      ),
    ).toBeUndefined();
    expect(
      parsePairingQrPayload(
        JSON.stringify({
          ...payload,
          ws_url: "http://not-supported",
        }),
      ),
    ).toBeUndefined();
  });

  it("QR pairing payload 也应接受单行邀请码", () => {
    const payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 2,
      token: "pair-token",
      server_id: "00000000-0000-0000-0000-000000000001",
      expires_at_ms: 1710000060000,
    };
    const inviteCode = pairingInviteCode(payload, 2);

    expect(inviteCode).toMatch(/^termd-pair:v2:/);
    expect(parsePairingQrPayload(inviteCode)).toEqual(payload);
  });
});

function pairingInviteCode(payload: PairingQrPayload, version: 1 | 2 = 1): string {
  const json = JSON.stringify(payload);
  const encoded = Buffer.from(json, "utf8").toString("base64url");
  return `termd-pair:v${version}:${encoded}`;
}
