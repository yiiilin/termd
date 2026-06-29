import type { Envelope } from "./types";

interface ProtocolMethodRegistryEntry {
  legacyEnvelopeType: Envelope["type"];
  needsEmptyAck?: boolean;
}

// 中文注释：这里集中维护 packet method 与旧 envelope 名称的兼容映射；
// DirectClient 仍只暴露 method API，UI 层不需要知道旧 envelope 细节。
export const PROTOCOL_METHOD_REGISTRY = {
  "pair.request": { legacyEnvelopeType: "pair_request" },
  auth: { legacyEnvelopeType: "auth", needsEmptyAck: true },
  "auth.verify": { legacyEnvelopeType: "auth", needsEmptyAck: true },
  "auth.session_token": { legacyEnvelopeType: "session_token_grant" },
  "session.scope_token": { legacyEnvelopeType: "session_scope_grant" },
  "client.hello": { legacyEnvelopeType: "client_hello", needsEmptyAck: true },
  "session.create": { legacyEnvelopeType: "session_create" },
  "session.list": { legacyEnvelopeType: "session_list" },
  "daemon.clients": { legacyEnvelopeType: "daemon_clients" },
  "daemon.client_forget": { legacyEnvelopeType: "daemon_client_forget" },
  "daemon.status": { legacyEnvelopeType: "daemon_status" },
  "metadata.subscribe": { legacyEnvelopeType: "metadata_subscribe", needsEmptyAck: true },
  "terminal.create": { legacyEnvelopeType: "session_create" },
  "session.attach": { legacyEnvelopeType: "session_attach" },
  "terminal.attach": { legacyEnvelopeType: "session_attach" },
  "session.data": { legacyEnvelopeType: "session_data" },
  "session.cursor": { legacyEnvelopeType: "session_cursor", needsEmptyAck: true },
  "session.resize": { legacyEnvelopeType: "session_resize" },
  "session.rename": { legacyEnvelopeType: "session_rename" },
  "session.reorder": { legacyEnvelopeType: "session_reorder" },
  "session.close": { legacyEnvelopeType: "session_close" },
  "session.files": { legacyEnvelopeType: "session_files" },
  "session.search": { legacyEnvelopeType: "session_search" },
  "session.git": { legacyEnvelopeType: "session_git" },
  "session.git_diff": { legacyEnvelopeType: "session_git_diff" },
  "session.git_action": { legacyEnvelopeType: "session_git_action" },
  "session.file_read": { legacyEnvelopeType: "session_file_read" },
  "session.file_download_prepare": { legacyEnvelopeType: "session_file_download_prepare" },
  "session.file_download_chunk": { legacyEnvelopeType: "session_file_download_chunk" },
  "session.file_write": { legacyEnvelopeType: "session_file_write" },
  "session.file_delete": { legacyEnvelopeType: "session_file_delete" },
  "control.request": { legacyEnvelopeType: "control_request" },
  ping: { legacyEnvelopeType: "ping" },
} as const satisfies Record<string, ProtocolMethodRegistryEntry>;

export type ProtocolMethod = keyof typeof PROTOCOL_METHOD_REGISTRY;

// 中文注释：event method 是 packet 模式的公开名字，envelope type 是旧 JSON 内层名字。
export const PROTOCOL_EVENT_METHOD_REGISTRY = {
  "auth.challenge": "auth_challenge",
  "auth.session_token": "session_token_grant",
  "session.scope_token": "session_scope_grant",
  "session.activity": "session_activity",
  "session.files": "session_files_result",
  "session.cwd": "session_cwd_changed",
  "session.git": "session_git_result",
  "session.closed": "session_closed",
  "session.resized": "session_resized",
  "terminal.output": "session_data",
  "daemon.clients_snapshot": "daemon_clients_snapshot",
  "daemon.status_snapshot": "daemon_status_snapshot",
} as const satisfies Record<string, Envelope["type"]>;

export type ProtocolEventMethod = keyof typeof PROTOCOL_EVENT_METHOD_REGISTRY;

const PROTOCOL_EVENT_METHOD_BY_LEGACY_ENVELOPE_TYPE = Object.fromEntries(
  Object.entries(PROTOCOL_EVENT_METHOD_REGISTRY).map(([method, type]) => [type, method]),
) as Partial<Record<Envelope["type"], ProtocolEventMethod>>;

export function legacyEnvelopeTypeForProtocolMethod(method?: string): Envelope["type"] | undefined {
  if (!method) {
    return undefined;
  }
  const entry = PROTOCOL_METHOD_REGISTRY[method as ProtocolMethod] as ProtocolMethodRegistryEntry | undefined;
  return entry?.legacyEnvelopeType;
}

export function protocolMethodNeedsEmptyAck(method?: string): boolean {
  if (!method) {
    return false;
  }
  const entry = PROTOCOL_METHOD_REGISTRY[method as ProtocolMethod] as ProtocolMethodRegistryEntry | undefined;
  return Boolean(entry?.needsEmptyAck);
}

export function envelopeTypeForProtocolEventMethod(method?: string): Envelope["type"] | undefined {
  if (!method) {
    return undefined;
  }
  return PROTOCOL_EVENT_METHOD_REGISTRY[method as ProtocolEventMethod];
}

export function protocolEventMethodForLegacyEnvelopeType(type: Envelope["type"]): string {
  return PROTOCOL_EVENT_METHOD_BY_LEGACY_ENVELOPE_TYPE[type] ?? type.replaceAll("_", ".");
}
