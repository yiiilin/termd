import { describe, expect, it } from "vitest";
import {
  ALL_MESSAGE_TYPES,
  type AttachRole,
  type DaemonClientSummaryPayload,
  type Envelope,
  type PairingQrPayload,
  type MessageType,
  type RouteHelloPayload,
  type RouteReadyPayload,
  type RouteRole,
  type SessionCursorPayload,
  type SessionState,
} from "../protocol/types";
import { parsePairingQrPayload } from "../protocol/pairing-payload";
import { envelope } from "../protocol/wire";

describe("协议类型", () => {
  it("消息类型使用 Rust proto 的 snake_case wire 名称", () => {
    const expected: MessageType[] = [
      "route_hello",
      "route_ready",
      "hello",
      "auth",
      "auth_challenge",
      "pair_request",
      "pair_accept",
      "session_create",
      "session_created",
      "session_attach",
      "session_attached",
      "session_data",
      "session_cursor",
      "session_resize",
      "session_rename",
      "session_renamed",
      "session_close",
      "session_closed",
      "session_files",
      "session_files_result",
      "session_file_read",
      "session_file_read_result",
      "session_file_write",
      "session_file_written",
      "session_file_delete",
      "session_file_deleted",
      "session_list",
      "session_list_result",
      "client_hello",
      "daemon_clients",
      "daemon_clients_result",
      "daemon_client_forget",
      "daemon_client_forgot",
      "control_request",
      "control_grant",
      "e2ee_key_exchange",
      "encrypted_frame",
      "error",
      "ping",
      "pong",
    ];

    expect(ALL_MESSAGE_TYPES).toEqual(expected);
  });

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
    const routeRoles: RouteRole[] = ["client", "daemon_mux"];
    const forbidden = ["admin", "owner", "member", "tenant"];

    expect(states).toEqual(["created", "running", "closed"]);
    expect(roles).toEqual(["operator"]);
    expect(routeRoles).toEqual(["client", "daemon_mux"]);
    expect(JSON.stringify({ states, roles }).toLowerCase()).not.toContain(forbidden.join("|"));
  });

  it("route prelude payload 只携带公开路由字段", () => {
    const routeHello: RouteHelloPayload = {
      server_id: "00000000-0000-0000-0000-000000000001",
      role: "client",
      protocol_version: 1,
      nonce: "route-nonce",
      timestamp_ms: 1_710_000_000_000,
    };
    const routeReady: RouteReadyPayload = {
      server_id: routeHello.server_id,
      role: "client",
    };

    expect(envelope("route_hello", routeHello)).toEqual({
      type: "route_hello",
      payload: routeHello,
    });
    expect(envelope("route_ready", routeReady)).toEqual({
      type: "route_ready",
      payload: routeReady,
    });
    expect(JSON.stringify(routeHello)).not.toContain("token");
  });

  it("TypeScript wire shape 和 JSON envelope 可序列化", () => {
    const message: Envelope = envelope("session_list", {});
    const raw = JSON.stringify(message);

    expect(raw).toBe('{"type":"session_list","payload":{}}');
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

  it("QR pairing payload 只在有效 JSON 且带 token/server_id 时被识别，ws_url 仅作旧版兼容", () => {
    const payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 1,
      token: "pair-token",
      server_id: "00000000-0000-0000-0000-000000000001",
      expires_at_ms: 1710000060000,
    };

    expect(parsePairingQrPayload(JSON.stringify(payload))).toEqual(payload);
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
          ws_url: "http://not-supported",
        }),
      ),
    ).toBeUndefined();
  });

  it("QR pairing payload 也应接受单行邀请码", () => {
    const payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 1,
      token: "pair-token",
      server_id: "00000000-0000-0000-0000-000000000001",
      expires_at_ms: 1710000060000,
    };
    const inviteCode = pairingInviteCode(payload);

    expect(inviteCode).toMatch(/^termd-pair:v1:/);
    expect(parsePairingQrPayload(inviteCode)).toEqual(payload);
  });
});

function pairingInviteCode(payload: PairingQrPayload): string {
  const json = JSON.stringify(payload);
  const encoded = Buffer.from(json, "utf8").toString("base64url");
  return `termd-pair:v1:${encoded}`;
}
