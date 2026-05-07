import { describe, expect, it } from "vitest";
import {
  ALL_MESSAGE_TYPES,
  type AttachRole,
  type Envelope,
  type PairingQrPayload,
  type MessageType,
  type SessionState,
} from "../protocol/types";
import { parsePairingQrPayload } from "../protocol/pairing-payload";
import { envelope } from "../protocol/wire";

describe("协议类型", () => {
  it("消息类型使用 Rust proto 的 snake_case wire 名称", () => {
    const expected: MessageType[] = [
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
      "session_resize",
      "session_list",
      "session_list_result",
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

  it("状态枚举不引入用户或 RBAC 语义", () => {
    const states: SessionState[] = ["created", "running", "closed"];
    const roles: AttachRole[] = ["controller", "viewer"];
    const forbidden = ["admin", "owner", "member", "rbac"];

    expect(states).toEqual(["created", "running", "closed"]);
    expect(roles).toEqual(["controller", "viewer"]);
    expect(JSON.stringify({ states, roles }).toLowerCase()).not.toContain(forbidden.join("|"));
  });

  it("TypeScript wire shape 和 JSON envelope 可序列化", () => {
    const message: Envelope = envelope("session_list", {});
    const raw = JSON.stringify(message);

    expect(raw).toBe('{"type":"session_list","payload":{}}');
  });

  it("QR pairing payload 只在有效 JSON 且带 ws_url/token/server_id 时被识别", () => {
    const payload: PairingQrPayload = {
      type: "termd_pairing_qr",
      version: 1,
      ws_url: "wss://relay.example/ws/00000000-0000-0000-0000-000000000001/client",
      token: "pair-token",
      server_id: "00000000-0000-0000-0000-000000000001",
      expires_at_ms: 1710000060000,
    };

    expect(parsePairingQrPayload(JSON.stringify(payload))).toEqual(payload);
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
});
