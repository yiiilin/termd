export const ALL_MESSAGE_TYPES = [
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
  "daemon_clients",
  "daemon_clients_result",
  "control_request",
  "control_grant",
  "e2ee_key_exchange",
  "encrypted_frame",
  "error",
  "ping",
  "pong",
] as const;

export type MessageType = (typeof ALL_MESSAGE_TYPES)[number];

export type UUID = string;
export type PublicKeyWire = string;
export type Nonce = string;
export type Challenge = string;
export type SignatureWire = string;
export type UnixTimestampMillis = number;

export interface Envelope<P = unknown> {
  type: MessageType;
  payload: P;
}

export interface HelloPayload {
  protocol_version: number;
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
  server_id: UUID | null;
  device_id: UUID | null;
}

export interface E2eeKeyExchangePayload {
  server_id: UUID;
  device_id: UUID;
  public_key: PublicKeyWire;
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
}

export interface EncryptedFramePayload {
  server_id: UUID;
  sequence: number;
  ciphertext_base64: string;
}

export interface PairRequestPayload {
  device_id: UUID;
  device_public_key: PublicKeyWire;
  token: string;
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
}

export interface PairAcceptPayload {
  server_id: UUID;
  daemon_public_key: PublicKeyWire;
  device_id: UUID;
  expires_at_ms: UnixTimestampMillis;
}

export interface PairingQrPayload {
  type: "termd_pairing_qr";
  version: 1;
  ws_url: string;
  token: string;
  server_id: UUID;
  expires_at_ms: UnixTimestampMillis;
}

export interface AuthChallengePayload {
  device_id: UUID;
  challenge: Challenge;
  expires_at_ms: UnixTimestampMillis;
}

export interface AuthPayload {
  device_id: UUID;
  challenge: Challenge;
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
  signature: SignatureWire;
}

export interface TerminalSize {
  rows: number;
  cols: number;
  pixel_width: number;
  pixel_height: number;
}

export type SessionState = "created" | "running" | "closed";
export type AttachRole = "operator";

export interface SessionSummaryPayload {
  session_id: UUID;
  name?: string | null;
  state: SessionState;
  size: TerminalSize;
}

export interface SessionListResultPayload {
  sessions: SessionSummaryPayload[];
}

export interface DaemonClientsPayload {}

export interface DaemonClientSummaryPayload {
  client_id: UUID;
  device_id: UUID;
  peer_ip: string | null;
  online: boolean;
  connected_at_ms: UnixTimestampMillis;
  last_seen_at_ms: UnixTimestampMillis;
  attached_session_ids: UUID[];
  cursor_session_id?: UUID | null;
  cursor_row?: number | null;
  cursor_col?: number | null;
}

export interface DaemonClientsResultPayload {
  clients: DaemonClientSummaryPayload[];
}

export interface SessionCreatePayload {
  command: string[];
  size: TerminalSize;
}

export interface SessionCreatedPayload {
  session_id: UUID;
  role: AttachRole;
  state: SessionState;
  size: TerminalSize;
}

export interface SessionAttachPayload {
  session_id: UUID;
}

export interface SessionAttachedPayload {
  session_id: UUID;
  role: AttachRole;
  state: SessionState;
  size: TerminalSize;
}

export interface SessionRenamePayload {
  session_id: UUID;
  name: string;
}

export interface SessionRenamedPayload {
  session_id: UUID;
  name: string;
}

export interface SessionClosePayload {
  session_id: UUID;
}

export interface SessionClosedPayload {
  session_id: UUID;
}

export type SessionFileKind = "file" | "directory" | "symlink" | "other";

export interface SessionFilesPayload {
  session_id: UUID;
  path?: string | null;
}

export interface SessionFileEntryPayload {
  name: string;
  path: string;
  kind: SessionFileKind;
  size_bytes: number;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFilesResultPayload {
  session_id: UUID;
  path: string;
  entries: SessionFileEntryPayload[];
}

export interface SessionFileReadPayload {
  session_id: UUID;
  path: string;
}

export interface SessionFileReadResultPayload {
  session_id: UUID;
  path: string;
  data_base64: string;
  size_bytes: number;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFileWritePayload {
  session_id: UUID;
  path: string;
  data_base64: string;
}

export interface SessionFileWrittenPayload {
  session_id: UUID;
  path: string;
  size_bytes: number;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFileDeletePayload {
  session_id: UUID;
  path: string;
}

export interface SessionFileDeletedPayload {
  session_id: UUID;
  path: string;
}

export interface SessionDataPayload {
  session_id: UUID;
  data_base64: string;
}

export interface SessionCursorPayload {
  session_id: UUID;
  row: number;
  col: number;
}

export interface ControlGrantPayload {
  session_id: UUID;
  device_id: UUID;
}

export interface ErrorPayload {
  code: string;
  message: string;
}

export interface PingPayload {
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
}

export interface PongPayload {
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
}

export interface DeviceState {
  device_id: UUID;
  device_public_key: PublicKeyWire;
  device_signing_key_secret: string;
}

export interface PairedServerState {
  server_id: UUID;
  daemon_public_key: PublicKeyWire;
  url: string;
  paired_at_ms: UnixTimestampMillis;
}

export interface BrowserState {
  device?: DeviceState;
  pairedServers: PairedServerState[];
  defaultServerId?: UUID;
  defaultUrl?: string;
}

export interface SafeError {
  code: string;
  message: string;
}
