export const ALL_MESSAGE_TYPES = [
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
  "terminal_frame",
  "session_activity",
  "session_cwd_changed",
  "session_cursor",
  "session_resize",
  "session_resized",
  "session_rename",
  "session_renamed",
  "session_reorder",
  "session_reordered",
  "session_close",
  "session_closed",
  "session_search",
  "session_search_result",
  "session_files",
  "session_files_result",
  "session_git",
  "session_git_result",
  "session_git_action",
  "session_git_action_result",
  "session_git_diff",
  "session_git_diff_result",
  "session_file_read",
  "session_file_read_result",
  "session_file_write",
  "session_file_written",
  "session_file_delete",
  "session_file_deleted",
  "session_file_download_prepare",
  "session_file_download_ready",
  "session_file_download_chunk",
  "session_file_download_chunk_result",
  "session_list",
  "session_list_result",
  "client_hello",
  "daemon_clients",
  "daemon_clients_result",
  "daemon_client_forget",
  "daemon_client_forgot",
  "daemon_status",
  "daemon_status_result",
  "control_request",
  "control_grant",
  "e2ee_key_exchange",
  "encrypted_frame",
  "packet",
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
export type PacketRequestId = UUID;
export type PacketStreamId = UUID;

export interface Envelope<P = unknown> {
  type: MessageType;
  payload: P;
}

export const PROTOCOL_PACKET_VERSION = 3;
export const BINARY_PROTOCOL_VERSION = 2;

export type PacketKind =
  | "request"
  | "response"
  | "event"
  | "stream_open"
  | "stream_chunk"
  | "stream_end"
  | "cancel"
  | "flow"
  | "error";

export interface PacketErrorPayload {
  code: string;
  message: string;
  retryable: boolean;
}

export interface ProtocolPacket<P = unknown> {
  version: number;
  kind: PacketKind;
  id?: PacketRequestId;
  stream_id?: PacketStreamId;
  method?: string;
  seq?: number;
  ack?: number;
  credit?: number;
  payload: P;
}

export type RouteRole = "client" | "daemon_mux";

export interface RouteHelloPayload {
  server_id: UUID;
  role: RouteRole;
  protocol_version: number;
  nonce: Nonce;
  route_generation?: Nonce;
  timestamp_ms: UnixTimestampMillis;
}

export interface RouteReadyPayload {
  server_id: UUID;
  role: RouteRole;
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
  packet_version?: number | null;
  binary_version?: number | null;
  signature?: SignatureWire | null;
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
  ws_url?: string;
  token: string;
  server_id: UUID;
  daemon_public_key?: PublicKeyWire;
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

export interface HttpE2eeAuthPayload {
  device_id: UUID;
  e2ee_public_key: PublicKeyWire;
  nonce: Nonce;
  timestamp_ms: UnixTimestampMillis;
  method: string;
  path: string;
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
  files_path?: string | null;
  created_at_ms?: UnixTimestampMillis | null;
}

export interface SessionListResultPayload {
  sessions: SessionSummaryPayload[];
}

export interface DaemonClientsPayload {}

export interface ClientHelloPayload {
  name: string;
}

export interface DaemonClientSummaryPayload {
  client_id: UUID;
  device_id: UUID;
  name?: string | null;
  peer_ip: string | null;
  online: boolean;
  connected_at_ms: UnixTimestampMillis;
  last_seen_at_ms: UnixTimestampMillis;
  attached_session_ids: UUID[];
  cursor_session_id?: UUID | null;
  cursor_row?: number | null;
  cursor_col?: number | null;
  cursor_focused?: boolean | null;
}

export interface DaemonClientsResultPayload {
  clients: DaemonClientSummaryPayload[];
}

export interface DaemonClientForgetPayload {
  device_id: UUID;
}

export interface DaemonClientForgotPayload {
  device_id: UUID;
}

export interface DaemonStatusPayload {}

export interface DaemonStatusResultPayload {
  host_name?: string | null;
  load_avg: [number, number, number];
  uptime_seconds: number;
  cpu_percent: number;
  memory_total_bytes: number;
  memory_available_bytes: number;
  disk_total_bytes: number;
  disk_available_bytes: number;
  network_rx_bytes?: number;
  network_tx_bytes?: number;
  /**
   * 兼容旧 daemon/status payload 的保留字段；当前状态栏不再显示进程数量。
   */
  process_count?: number;
  atop_available: boolean;
}

export interface SessionCreatePayload {
  command: string[];
  size: TerminalSize;
}

export interface SessionCreatedPayload {
  session_id: UUID;
  name?: string | null;
  role: AttachRole;
  state: SessionState;
  size: TerminalSize;
  resize_owner?: boolean;
}

export interface SessionAttachPayload {
  session_id: UUID;
  watch_updates?: boolean;
  last_terminal_seq?: number | null;
}

export interface SessionAttachedPayload {
  session_id: UUID;
  role: AttachRole;
  state: SessionState;
  size: TerminalSize;
  resize_owner?: boolean;
}

export interface SessionRenamePayload {
  session_id: UUID;
  name: string;
}

export interface SessionRenamedPayload {
  session_id: UUID;
  name: string;
}

export interface SessionReorderPayload {
  session_ids: UUID[];
}

export interface SessionReorderedPayload {
  session_ids: UUID[];
}

export interface SessionClosePayload {
  session_id: UUID;
}

export interface SessionClosedPayload {
  session_id: UUID;
}

export interface SessionSearchPayload {
  session_id: UUID;
  query: string;
  case_sensitive?: boolean;
  max_results?: number | null;
}

export interface SessionSearchMatchPayload {
  line_index: number;
  column_index: number;
  line_text: string;
}

export interface SessionSearchResultPayload {
  session_id: UUID;
  query: string;
  line_count: number;
  matches: SessionSearchMatchPayload[];
  truncated: boolean;
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

export interface SessionGitPayload {
  session_id: UUID;
}

export interface SessionGitFileChangePayload {
  path: string;
  status: string;
}

export interface SessionGitWorktreePayload {
  path: string;
  branch?: string | null;
  head?: string | null;
  is_current: boolean;
  staged: SessionGitFileChangePayload[];
  unstaged: SessionGitFileChangePayload[];
}

export interface SessionGitResultPayload {
  session_id: UUID;
  cwd: string;
  repository_root?: string | null;
  worktrees: SessionGitWorktreePayload[];
  graph: string[];
  error?: string | null;
}

export type SessionGitActionKind = "stage" | "unstage" | "discard";

export interface SessionGitActionPayload {
  session_id: UUID;
  worktree_path: string;
  file_path: string;
  action: SessionGitActionKind;
}

export interface SessionGitActionResultPayload {
  session_id: UUID;
  worktree_path: string;
  file_path: string;
  action: SessionGitActionKind;
}

export interface SessionGitDiffPayload {
  session_id: UUID;
  worktree_path: string;
  file_path?: string | null;
  staged?: boolean;
}

export interface SessionGitDiffResultPayload {
  session_id: UUID;
  worktree_path: string;
  file_path?: string | null;
  staged: boolean;
  diff: string;
}

export interface SessionFileReadPayload {
  session_id: UUID;
  path: string;
  max_bytes?: number;
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

export interface SessionFileDownloadPreparePayload {
  session_id: UUID;
  path: string;
}

export interface SessionFileDownloadReadyPayload {
  session_id: UUID;
  path: string;
  token: string;
  size_bytes: number;
  modified_at_ms?: UnixTimestampMillis | null;
  expires_at_ms: UnixTimestampMillis;
}

export interface SessionFileDownloadChunkPayload {
  session_id: UUID;
  path: string;
  offset_bytes: number;
  max_bytes: number;
}

export interface SessionFileDownloadChunkResultPayload {
  session_id: UUID;
  path: string;
  offset_bytes: number;
  data_base64: string;
  next_offset_bytes: number;
  size_bytes: number;
  eof: boolean;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFileUploadPayload {
  session_id: UUID;
  path: string;
  size_bytes: number;
}

export interface SessionFileUploadReadyPayload {
  session_id: UUID;
  path: string;
  size_bytes: number;
  offset_bytes: number;
}

export interface SessionFileHttpUploadReadyPayload {
  session_id: UUID;
  path: string;
  upload_id: string;
  size_bytes: number;
  offset_bytes: number;
}

export interface SessionFileHttpUploadStreamPayload {
  session_id: UUID;
  path: string;
  upload_id: string;
  size_bytes: number;
  offset_bytes: number;
}

export interface SessionFileHttpDownloadPayload {
  session_id: UUID;
  path: string;
  offset_bytes?: number;
}

export interface SessionFileUploadProgressPayload {
  session_id: UUID;
  path: string;
  offset_bytes: number;
  size_bytes: number;
  eof: boolean;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFileDownloadStreamPayload {
  session_id: UUID;
  path: string;
}

export interface SessionFileDownloadStreamReadyPayload {
  session_id: UUID;
  path: string;
  name: string;
  size_bytes: number;
  modified_at_ms?: UnixTimestampMillis | null;
}

export interface SessionFileTransferChunkPayload {
  session_id: UUID;
  offset_bytes: number;
  data_base64?: string;
  data_bytes?: Uint8Array;
  size_bytes: number;
  eof: boolean;
}

export interface SessionDataPayload {
  session_id: UUID;
  data_base64?: string;
  data_bytes?: Uint8Array;
  /** 前端内部字段：packet stream 承载时用于调试和测试 stream 归属，不参与流控。 */
  stream_id?: PacketStreamId;
  transport_seq?: number;
}

export type TerminalFrameKind = "snapshot" | "output" | "resize" | "exit" | "batch";

export type TerminalFramePayload =
  | {
      kind: "snapshot";
      session_id: UUID;
      base_seq: number;
      size: TerminalSize;
      data_base64?: string;
      data_bytes?: Uint8Array;
    }
  | {
      kind: "output";
      session_id: UUID;
      terminal_seq: number;
      data_base64?: string;
      data_bytes?: Uint8Array;
    }
  | {
      kind: "resize";
      session_id: UUID;
      terminal_seq: number;
      size: TerminalSize;
    }
  | {
      kind: "exit";
      session_id: UUID;
      terminal_seq: number;
      code?: number | null;
    }
  | {
      kind: "batch";
      session_id: UUID;
      frames: TerminalFramePayload[];
    };

export type SingleTerminalFramePayload = Exclude<TerminalFramePayload, { kind: "batch" }>;

export type RenderableTerminalFramePayload = SingleTerminalFramePayload & {
  /** 前端内部字段：连接内 stream seq，用于调试和测试 stream 归属，不参与流控。 */
  transport_seq: number;
  /** 前端内部字段：当前 packet stream id，方便调试。 */
  stream_id: PacketStreamId;
};

export interface SessionActivityPayload {
  session_id: UUID;
  timestamp_ms: UnixTimestampMillis;
}

export interface SessionCwdChangedPayload {
  session_id: UUID;
  cwd: string;
}

export interface SessionCursorPayload {
  session_id: UUID;
  row: number;
  col: number;
  focused: boolean;
}

export type SessionCursorPresence = Pick<SessionCursorPayload, "row" | "col" | "focused">;

export interface SessionResizePayload {
  session_id: UUID;
  size: TerminalSize;
}

export interface SessionResizedPayload {
  session_id: UUID;
  size: TerminalSize;
  resize_owner?: boolean;
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
  name?: string | null;
}

export interface PairedServerState {
  server_id: UUID;
  daemon_public_key: PublicKeyWire;
  url: string;
  paired_at_ms: UnixTimestampMillis;
  name?: string | null;
}

export type BrowserLanguagePreference = "auto" | "zh-CN" | "en-US";
export type BrowserThemePreference = "system" | "dark" | "light";
export type EffectiveTheme = "dark" | "light";
export type BrowserNotificationPreference = "off" | "mentions" | "all";

export interface BrowserMobileShortcut {
  label: string;
  data: string;
}

export interface BrowserPreferences {
  language: BrowserLanguagePreference;
  theme: BrowserThemePreference;
  notifications?: BrowserNotificationPreference;
  mobileShortcuts?: BrowserMobileShortcut[];
}

export interface BrowserState {
  device?: DeviceState;
  pairedServers: PairedServerState[];
  defaultServerId?: UUID;
  defaultUrl?: string;
  preferences?: BrowserPreferences;
}

export interface SafeError {
  code: string;
  message: string;
}
