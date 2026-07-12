//! termd 的共享协议类型。
//!
//! 这个 crate 只描述客户端、daemon 与 relay 都需要知道的稳定外壳。
//! 具体业务规则仍由 daemon 执行，relay 只能基于公开路由/admission 字段做入口控制和转发。

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 所有 JSON 消息都使用同一个 envelope，避免不同层混用协议格式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<P = serde_json::Value> {
    /// wire 格式要求字段名为 `type`，Rust 中用 `kind` 避免关键字冲突。
    #[serde(rename = "type")]
    pub kind: MessageType,
    pub payload: P,
}

impl<P> Envelope<P> {
    /// 构造函数保持极薄，只负责协议外壳，不在这里做控制权判断。
    pub fn new(kind: MessageType, payload: P) -> Self {
        Self { kind, payload }
    }
}

fn default_true() -> bool {
    true
}

/// 文档中列出的 MVP 必备消息类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    RouteHello,
    RouteReady,
    Hello,
    Auth,
    AuthChallenge,
    PairRequest,
    PairAccept,
    SessionCreate,
    SessionCreated,
    SessionAttach,
    SessionAttached,
    SessionData,
    TerminalFrame,
    AttachFrame,
    SessionActivity,
    SessionCwdChanged,
    SessionCursor,
    SessionResize,
    SessionResized,
    SessionRename,
    SessionRenamed,
    SessionReorder,
    SessionReordered,
    SessionClose,
    SessionClosed,
    SessionSearch,
    SessionSearchResult,
    SessionFiles,
    SessionFilesResult,
    SessionGit,
    SessionGitResult,
    SessionGitAction,
    SessionGitActionResult,
    SessionGitDiff,
    SessionGitDiffResult,
    SessionFileRead,
    SessionFileReadResult,
    SessionFileWrite,
    SessionFileWritten,
    SessionFileDelete,
    SessionFileDeleted,
    SessionFileDownloadPrepare,
    SessionFileDownloadReady,
    SessionFileDownloadChunk,
    SessionFileDownloadChunkResult,
    SessionList,
    SessionListResult,
    ClientHello,
    DaemonClients,
    DaemonClientsResult,
    DaemonClientForget,
    DaemonClientForgot,
    DaemonStatus,
    DaemonStatusResult,
    MetadataSubscribe,
    DaemonClientsSnapshot,
    DaemonStatusSnapshot,
    ControlRequest,
    ControlGrant,
    Error,
    Ping,
    Pong,
}

/// daemon 的公开身份；它是信任根的可路由标识，不包含 server 私钥。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerId(pub Uuid);

impl ServerId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ServerId {
    fn default() -> Self {
        Self::new()
    }
}

/// session 标识只表示一个持久终端实例，不等同于设备身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// device key 对应的设备身份；真实验签由 auth 层完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

/// Web/CLI 侧展示用的客户端标识。
///
/// 对个人使用场景而言，客户端通常对应一个已配对设备/浏览器，而不是每次 attach 新建的
/// WebSocket 实例；它不代表账号权限或企业策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub Uuid);

impl ClientId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

/// session 状态机必须保持单向推进，不能从 CLOSED 恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Running,
    Closed,
}

/// connection 状态用于约束 attach 前必须先完成认证。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Init,
    Auth,
    Attached,
    Closed,
}

/// attach 后的运行角色。
///
/// shared-control 模式下所有已 attach 设备都是 operator，都可以向同一个 PTY 输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachRole {
    Operator,
}

/// 控制权状态只记录当前 holder，不引入平台级策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlState {
    None,
    Held { device_id: DeviceId },
}

impl ControlState {
    pub fn holder(&self) -> Option<DeviceId> {
        match self {
            Self::None => None,
            Self::Held { device_id } => Some(*device_id),
        }
    }
}

/// 协议版本先用整数表达，便于后续进行向后兼容判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u16);

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self(PROTOCOL_PACKET_VERSION)
    }
}

/// 公钥在 wire 层用字符串保存，具体编码由 auth/noise 层收敛。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicKey(pub String);

/// nonce 是 replay protection 的最小公共表达；生成和校验属于 auth 层。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Nonce(pub String);

/// challenge-response auth 需要在协议中显式携带挑战值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Challenge(pub String);

/// 签名内容和算法由 auth 层定义，proto 只固定 wire 边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Signature(pub String);

/// pairing token 必须有过期时间；token 明文只允许出现在 pairing invite 或已认证通道内。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairingToken(pub String);

/// 毫秒时间戳用于 replay protection 与 pairing token 过期判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTimestampMillis(pub u64);

/// 0.2.0 的加密业务包版本；外层 WebSocket/relay 仍只承担 transport。
pub const PROTOCOL_PACKET_VERSION: u16 = 3;

pub const METHOD_PAIR_REQUEST: &str = "pair.request";
pub const METHOD_AUTH: &str = "auth";
pub const METHOD_AUTH_VERIFY: &str = "auth.verify";
pub const METHOD_AUTH_CHALLENGE: &str = "auth.challenge";
pub const METHOD_CLIENT_HELLO: &str = "client.hello";
pub const METHOD_SESSION_CREATE: &str = "session.create";
pub const METHOD_SESSION_ATTACH: &str = "session.attach";
pub const METHOD_TERMINAL_CREATE: &str = "terminal.create";
pub const METHOD_TERMINAL_ATTACH: &str = "terminal.attach";
pub const METHOD_TERMINAL_OUTPUT: &str = "terminal.output";
pub const METHOD_SESSION_DATA: &str = "session.data";
pub const METHOD_SESSION_ACTIVITY: &str = "session.activity";
pub const METHOD_SESSION_CWD: &str = "session.cwd";
pub const METHOD_SESSION_CURSOR: &str = "session.cursor";
pub const METHOD_SESSION_RESIZE: &str = "session.resize";
pub const METHOD_SESSION_RESIZED: &str = "session.resized";
pub const METHOD_SESSION_RENAME: &str = "session.rename";
pub const METHOD_SESSION_REORDER: &str = "session.reorder";
pub const METHOD_SESSION_CLOSE: &str = "session.close";
pub const METHOD_SESSION_CLOSED: &str = "session.closed";
pub const METHOD_SESSION_SEARCH: &str = "session.search";
pub const METHOD_SESSION_FILES: &str = "session.files";
pub const METHOD_SESSION_GIT: &str = "session.git";
pub const METHOD_SESSION_GIT_ACTION: &str = "session.git_action";
pub const METHOD_SESSION_GIT_DIFF: &str = "session.git_diff";
pub const METHOD_SESSION_FILE_READ: &str = "session.file_read";
pub const METHOD_SESSION_FILE_WRITE: &str = "session.file_write";
pub const METHOD_SESSION_FILE_DELETE: &str = "session.file_delete";
pub const METHOD_SESSION_FILE_DOWNLOAD_PREPARE: &str = "session.file_download_prepare";
pub const METHOD_SESSION_FILE_DOWNLOAD_CHUNK: &str = "session.file_download_chunk";
pub const METHOD_SESSION_FILE_UPLOAD_STREAM: &str = "session.file_upload";
pub const METHOD_SESSION_FILE_DOWNLOAD_STREAM: &str = "session.file_download";
pub const METHOD_SESSION_LIST: &str = "session.list";
pub const METHOD_DAEMON_CLIENTS: &str = "daemon.clients";
pub const METHOD_DAEMON_CLIENT_FORGET: &str = "daemon.client_forget";
pub const METHOD_DAEMON_STATUS: &str = "daemon.status";
pub const METHOD_METADATA_SUBSCRIBE: &str = "metadata.subscribe";
pub const METHOD_DAEMON_CLIENTS_SNAPSHOT: &str = "daemon.clients_snapshot";
pub const METHOD_DAEMON_STATUS_SNAPSHOT: &str = "daemon.status_snapshot";
pub const METHOD_CONTROL_REQUEST: &str = "control.request";
pub const METHOD_PING: &str = "ping";

/// relay/daemon 共同使用的 HTTP tunnel 外层路由白名单。
///
/// 中文注释：这里不表达业务权限，只约束哪些 HTTP API 可以进入 relay/daemon tunnel。
/// bearer 仍在 daemon 内部验证；把路径白名单放在 proto crate 是为了
/// 避免 relay 和 daemon 分别手写一份字符串后发生协议面漂移。
pub fn is_http_tunnel_path_allowed(method: &str, path: &str) -> bool {
    let auth_path = matches!(
        path,
        "/api/auth/pair"
            | "/api/auth/challenge"
            | "/api/auth/access-token"
            | "/api/auth/device-certificate/migrate"
            | "/api/auth/device-certificate/migrate/challenge"
    );
    (method.eq_ignore_ascii_case("POST") && auth_path)
        || (method.eq_ignore_ascii_case("POST") && is_http_control_tunnel_path_allowed(path))
        || is_http_file_tunnel_path_allowed(method, path)
}

fn is_http_file_tunnel_path_allowed(method: &str, path: &str) -> bool {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["api", "files", "uploads"] | ["api", "files", "downloads"] => {
            method.eq_ignore_ascii_case("POST")
        }
        ["api", "files", "uploads", upload_id, "chunks"] => {
            !upload_id.is_empty() && method.eq_ignore_ascii_case("PUT")
        }
        ["api", "files", "uploads", upload_id, action] => {
            !upload_id.is_empty()
                && matches!(*action, "commit" | "abort")
                && method.eq_ignore_ascii_case("POST")
        }
        ["api", "files", "downloads", download_id] => {
            !download_id.is_empty() && method.eq_ignore_ascii_case("GET")
        }
        _ => false,
    }
}

pub fn is_http_control_tunnel_path_allowed(path: &str) -> bool {
    let segments = path
        .strip_prefix("/api/control/")
        .map(|trimmed| trimmed.split('/').collect::<Vec<_>>());
    let Some(segments) = segments else {
        return false;
    };
    match segments.as_slice() {
        ["session", "reorder"] => true,
        ["daemon", "client_forget"] => true,
        ["session", session_id, action] => {
            // 中文注释：session-scoped HTTP control path 必须在 allowlist 层确认 UUID。
            // 否则 `/api/control/session/not-a-uuid/files` 会绕过 404，提前进入认证/业务层。
            Uuid::parse_str(session_id).is_ok()
                && matches!(
                    *action,
                    "control"
                        | "rename"
                        | "close"
                        | "files"
                        | "search"
                        | "git"
                        | "git_diff"
                        | "git_action"
                        | "file_read"
                        | "file_write"
                        | "file_delete"
                )
        }
        _ => false,
    }
}

/// WebSocket 建立后的明文路由角色。
///
/// 这里只表达连接方向：relay 据此把连接放进对应 server_id 的房间；daemon 直连只接受
/// `client`。它不是终端 operator / viewer 权限模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteRole {
    Client,
    DaemonControl,
    DaemonData,
    DaemonMux,
}

/// WebSocket 第一帧路由前置握手。
///
/// 该消息携带公开的 server_id、连接方向，以及 trusted relay 入场所需的最小 admission
/// 凭证；不携带 session 数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteHelloPayload {
    pub server_id: ServerId,
    pub role: RouteRole,
    pub protocol_version: ProtocolVersion,
    pub nonce: Nonce,
    /// 可信 relay 在注册路由前使用的入场凭证。
    ///
    /// 中文注释：trusted relay 必须先确认连接是否允许进入对应 daemon 房间；
    /// termd 后续仍会对 pair/auth 做最终校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<RelayAdmissionPayload>,
    /// daemon mux 的公开连接代际。
    ///
    /// relay 只用它确认新 mux 是否替换旧 mux，不持有 daemon 业务状态。
    #[serde(default)]
    pub route_generation: Option<Nonce>,
    /// daemon data 连接要绑定的 relay client。
    ///
    /// browser client 和 daemon data 是一一配对的数据管道；relay 只用该字段做连接配对，
    /// 不解析后续业务内容。该字段为空时表示 daemon 预先建立的 idle data pipe，
    /// relay 会在后续 client 接入时通过公开生命周期帧把它分配给具体 client。
    #[serde(default)]
    pub client_id: Option<RelayClientId>,
    /// daemon data 连接的一次性配对令牌。
    ///
    /// 令牌由 relay 通过 daemon control 线下发；daemon 反连 data 线时带回。relay 用它
    /// 防止迟到的旧 data 连接误配到新的 browser client。idle data pipe 初始为空，真正
    /// 分配时仍会携带一次性令牌。
    #[serde(default)]
    pub data_token: Option<Nonce>,
    pub timestamp_ms: UnixTimestampMillis,
}

/// relay 入场凭证只用于 admission，不表达终端业务权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayAdmissionPayload {
    Daemon {
        token: String,
    },
    PairTicket {
        token: PairingToken,
    },
    Device {
        device_id: DeviceId,
        nonce: Nonce,
        timestamp_ms: UnixTimestampMillis,
        signature: Signature,
    },
}

/// routing prelude 通过后返回的确认消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteReadyPayload {
    pub server_id: ServerId,
    pub role: RouteRole,
}

/// hello 交换只表达身份、版本和防重放材料，不做认证决策。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloPayload {
    pub protocol_version: ProtocolVersion,
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
    pub server_id: Option<ServerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_public_key: Option<PublicKey>,
    pub device_id: Option<DeviceId>,
}

/// auth payload 支持设备级 challenge-response，不引入账号体系。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPayload {
    pub device_id: DeviceId,
    pub challenge: Challenge,
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
    pub signature: Signature,
}

/// daemon 在设备认证成功前发送给已配对设备的短期认证挑战。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallengePayload {
    pub device_id: DeviceId,
    pub challenge: Challenge,
    pub expires_at_ms: UnixTimestampMillis,
}

/// pair_request 用于新设备证明自己持有 pairing token 和设备公钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequestPayload {
    pub device_id: DeviceId,
    pub device_public_key: PublicKey,
    pub token: PairingToken,
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
}

/// pair_accept 返回 daemon 信任根和 token 过期信息，客户端不得保存 server 私钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairAcceptPayload {
    pub server_id: ServerId,
    pub daemon_public_key: PublicKey,
    pub device_id: DeviceId,
    pub expires_at_ms: UnixTimestampMillis,
}

/// 二维码 pairing 载荷只携带建立设备信任所需的 daemon 标识与短期 token。
///
/// token 仍然是敏感短期凭证；payload 不表达 operator 状态，也不包含任何私钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingQrPayload {
    #[serde(rename = "type")]
    pub payload_type: String,
    pub version: u16,
    /// 新邀请码默认不携带地址；Web 端使用当前页面地址，这里只保留旧邀请码兼容字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    pub token: PairingToken,
    pub server_id: ServerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_public_key: Option<PublicKey>,
    pub expires_at_ms: UnixTimestampMillis,
}

impl PairingQrPayload {
    pub const PAYLOAD_TYPE: &'static str = "termd_pairing_qr";
    pub const VERSION: u16 = 2;
    const INVITE_PREFIX: &'static str = "termd-pair:v2:";
    const LEGACY_INVITE_PREFIX: &'static str = "termd-pair:v1:";

    pub fn new(
        token: PairingToken,
        server_id: ServerId,
        expires_at_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            payload_type: Self::PAYLOAD_TYPE.to_owned(),
            version: Self::VERSION,
            ws_url: None,
            token,
            server_id,
            daemon_public_key: None,
            expires_at_ms,
        }
    }

    pub fn with_daemon_public_key(mut self, daemon_public_key: PublicKey) -> Self {
        self.daemon_public_key = Some(daemon_public_key);
        self
    }

    pub fn is_supported_version(&self) -> bool {
        self.payload_type == Self::PAYLOAD_TYPE && matches!(self.version, 1 | Self::VERSION)
    }

    /// 把 pairing payload 压成单行邀请码，便于复制粘贴和 QR 承载。
    ///
    /// 这不是安全加密，只是把结构化 JSON 包一层 URL-safe base64。
    pub fn to_invite_code(&self) -> String {
        let raw = serde_json::to_vec(self).expect("PairingQrPayload should serialize");
        format!("{}{}", Self::INVITE_PREFIX, URL_SAFE_NO_PAD.encode(raw))
    }

    /// 解析单行邀请码。
    ///
    /// 这里同时保留对旧 JSON 文本的兼容，便于平滑迁移已有复制流程。
    pub fn parse_invite_code(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if let Some(encoded) = trimmed
            .strip_prefix(Self::INVITE_PREFIX)
            .or_else(|| trimmed.strip_prefix(Self::LEGACY_INVITE_PREFIX))
        {
            let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
            let payload: Self = serde_json::from_slice(&bytes).ok()?;
            return payload.is_supported_version().then_some(payload);
        }

        let payload: Self = serde_json::from_str(trimmed).ok()?;
        payload.is_supported_version().then_some(payload)
    }
}

/// 终端尺寸同时保留像素信息，便于 GUI 客户端传递精确 resize。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalSize {
    pub const DEFAULT_ROWS: u16 = 24;
    pub const DEFAULT_COLS: u16 = 80;

    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(Self::DEFAULT_ROWS, Self::DEFAULT_COLS)
    }
}

/// 创建持久终端 session 的请求。空 command 表示使用 daemon 默认命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreatePayload {
    pub command: Vec<String>,
    pub size: TerminalSize,
}

/// session 创建成功后的 wire 响应；创建连接会立即 attach。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreatedPayload {
    pub session_id: SessionId,
    /// daemon 分配的稳定展示名；旧客户端/旧 daemon 可能缺失，前端需保留兜底显示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub role: AttachRole,
    pub state: SessionState,
    pub size: TerminalSize,
    /// 当前连接是否持有该 session 的 PTY resize 权限；shared-control 输入权限不受影响。
    #[serde(default)]
    pub resize_owner: bool,
}

/// attach 成功后的响应；shared-control 模式下 role 固定为 operator。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachedPayload {
    pub session_id: SessionId,
    pub role: AttachRole,
    pub state: SessionState,
    pub size: TerminalSize,
    /// 当前连接是否持有该 session 的 PTY resize 权限；后续 attach 连接默认只能 viewer/zoom。
    #[serde(default)]
    pub resize_owner: bool,
}

/// 列出 daemon 当前已知 session；MVP 暂不携带筛选条件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionListPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummaryPayload {
    pub session_id: SessionId,
    #[serde(default)]
    pub name: Option<String>,
    pub state: SessionState,
    pub size: TerminalSize,
    /// session 级共享文件树位置；为空时客户端应向 daemon 请求默认目录。
    #[serde(default)]
    pub files_path: Option<String>,
    /// 创建时间只用于客户端排序和人类可读展示，不参与权限或路由判断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResultPayload {
    pub sessions: Vec<SessionSummaryPayload>,
}

/// 查询当前 daemon 曾经见过或正在连接的客户端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonClientsPayload {}

/// 已认证客户端上报的人类可读展示信息。
///
/// 这不是认证材料；device id 和签名仍然是唯一可信身份。name 只用于 UI 区分多个浏览器。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHelloPayload {
    pub name: String,
    /// 连接用途只影响 daemon 的展示/历史记录策略，不参与鉴权或控制权判断。
    #[serde(default)]
    pub kind: ClientHelloKind,
}

/// 已认证 WebSocket 的展示用途。
///
/// metadata sidecar 是同一设备的旁路状态通道，不应该在 operator 列表里显示成第二个客户端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientHelloKind {
    #[default]
    Interactive,
    Metadata,
}

/// daemon 下单个客户端的展示摘要。
///
/// 该结构只用于个人视图里的连接可见性，不表达审计、账号或企业权限模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonClientSummaryPayload {
    pub client_id: ClientId,
    pub device_id: DeviceId,
    #[serde(default)]
    pub name: Option<String>,
    pub peer_ip: Option<String>,
    pub online: bool,
    pub connected_at_ms: UnixTimestampMillis,
    pub last_seen_at_ms: UnixTimestampMillis,
    pub attached_session_ids: Vec<SessionId>,
    /// 当前客户端最后上报光标所在的 session；离线或未 attach 时为空。
    #[serde(default)]
    pub cursor_session_id: Option<SessionId>,
    /// Web 终端 renderer 上报的 1-based 行号，用于顶部 operator 列表展示。
    #[serde(default)]
    pub cursor_row: Option<u16>,
    /// Web 终端 renderer 上报的 1-based 列号，用于顶部 operator 列表展示。
    #[serde(default)]
    pub cursor_col: Option<u16>,
    /// 当前 Web 终端是否处于聚焦状态；true 表示闪烁方块，false 表示非聚焦轮廓。
    #[serde(default)]
    pub cursor_focused: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonClientsResultPayload {
    pub clients: Vec<DaemonClientSummaryPayload>,
}

/// 删除 daemon 里的离线客户端历史记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonClientForgetPayload {
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonClientForgotPayload {
    pub device_id: DeviceId,
}

/// 查询 daemon 所在服务器的轻量运行状态。
///
/// 当前 metadata WebSocket 会把状态作为 snapshot/event 推送，trusted relay 可以看到应用流量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusPayload {}

/// 订阅 daemon 旁路 metadata 推送。
///
/// clients 为 false 时 daemon 只推 status；status_interval_ms 为空时不推周期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataSubscribePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_interval_ms: Option<u64>,
    #[serde(default)]
    pub clients: bool,
}

/// daemon 所在服务器的只读状态快照。
///
/// Linux 上优先来自 /proc 和根目录 statvfs；采集失败或非 Linux 平台使用 0/null 降级，
/// 避免状态面板影响终端主链路。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatusResultPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    pub load_avg: [f64; 3],
    pub uptime_seconds: u64,
    pub cpu_percent: f32,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_available_bytes: u64,
    /// 所有物理网卡累计接收字节数；旧 daemon 没有该字段时客户端按 0 降级。
    #[serde(default)]
    pub network_rx_bytes: u64,
    /// 所有物理网卡累计发送字节数；前端用相邻两次快照计算上下行速度。
    #[serde(default)]
    pub network_tx_bytes: u64,
    /// 兼容旧前端的保留字段；新状态栏不再展示进程数量。
    #[serde(default)]
    pub process_count: u64,
    pub atop_available: bool,
}

/// `session_data` 在 JSON 通道中使用 base64；二进制 WebSocket 帧可绕过这个结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDataPayload {
    pub session_id: SessionId,
    pub data_base64: String,
}

/// attach-scoped opaque terminal frame。
///
/// 中文注释：这层 payload 只把 frame 绑定到 session/stream；真正的终端业务语义由
/// `supervisor` 内部 length-prefixed JSON frame 定义，`termd` / `relay` 不应继续解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachFramePayload {
    pub session_id: SessionId,
    pub data_base64: String,
}

/// terminal stream 内的输出帧类型。
///
/// `terminal_seq` / `base_seq` 是 session 级终端事件序号，用于 snapshot 和 tail 的一致性判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFrameKind {
    Snapshot,
    Output,
    Resize,
    Exit,
    Batch,
}

/// packet terminal stream 的结构化帧。
///
/// snapshot 是替换语义，浏览器必须先 reset 当前 terminal renderer 再写入；output/resize/exit
/// 是 `base_seq` 之后的增量 tail。不要把 snapshot 伪装成普通 `session_data`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalFramePayload {
    Snapshot {
        session_id: SessionId,
        base_seq: u64,
        size: TerminalSize,
        data_base64: String,
    },
    Output {
        session_id: SessionId,
        terminal_seq: u64,
        data_base64: String,
    },
    Resize {
        session_id: SessionId,
        terminal_seq: u64,
        size: TerminalSize,
    },
    Exit {
        session_id: SessionId,
        terminal_seq: u64,
        code: Option<i32>,
    },
    Batch {
        session_id: SessionId,
        frames: Vec<TerminalFramePayload>,
    },
}

impl TerminalFramePayload {
    pub fn session_id(&self) -> SessionId {
        match self {
            Self::Snapshot { session_id, .. }
            | Self::Output { session_id, .. }
            | Self::Resize { session_id, .. }
            | Self::Exit { session_id, .. }
            | Self::Batch { session_id, .. } => *session_id,
        }
    }

    pub fn terminal_seq(&self) -> Option<u64> {
        match self {
            Self::Output { terminal_seq, .. }
            | Self::Resize { terminal_seq, .. }
            | Self::Exit { terminal_seq, .. } => Some(*terminal_seq),
            Self::Batch { frames, .. } => frames.iter().filter_map(Self::terminal_seq).next_back(),
            Self::Snapshot { .. } => None,
        }
    }
}

/// session 有新输出的轻量通知。
///
/// 该消息只用于 UI 标记后台 session，不携带终端明文，避免为了列表变色额外推送大块输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityPayload {
    pub session_id: SessionId,
    pub timestamp_ms: UnixTimestampMillis,
}

/// session 当前终端 cwd 已变化的轻量通知。
///
/// daemon / supervisor 只推送路径事实；更重的文件树/Git 明细仍由客户端按需向 termd 拉取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCwdChangedPayload {
    pub session_id: SessionId,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachPayload {
    pub session_id: SessionId,
    /// 是否订阅终端输出、文件树和 resize 推送；短连接 RPC 只需要权限时可以关闭。
    #[serde(default = "default_true")]
    pub watch_updates: bool,
    /// 客户端最后完成渲染的 session 级 terminal_seq；daemon/supervisor 用它决定补 tail 还是发 snapshot。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_terminal_seq: Option<u64>,
}

/// Web 客户端在 shared-control 顶部状态条中展示的光标位置。
///
/// 该 payload 只表达客户端本地可见的终端光标，不授予权限，也不参与 PTY 写入判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCursorPayload {
    pub session_id: SessionId,
    pub row: u16,
    pub col: u16,
    /// 兼容旧客户端：缺省时按未聚焦处理。
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizePayload {
    pub session_id: SessionId,
    pub size: TerminalSize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizedPayload {
    pub session_id: SessionId,
    pub size: TerminalSize,
    /// 接收该消息的连接是否仍是 resize owner；用于 owner 断开后的接管通知。
    #[serde(default)]
    pub resize_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamePayload {
    pub session_id: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamedPayload {
    pub session_id: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReorderPayload {
    pub session_ids: Vec<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReorderedPayload {
    pub session_ids: Vec<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosePayload {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosedPayload {
    pub session_id: SessionId,
}

/// 在 daemon 内存中的终端 screen snapshot 里搜索文本。
///
/// 这里刻意不要求 daemon 把 PTY 明文写入磁盘；搜索范围只覆盖当前进程保留的最近逻辑行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchPayload {
    pub session_id: SessionId,
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    /// 客户端可传入较小上限，避免一次搜索把大量 scrollback 明文推回浏览器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchMatchPayload {
    /// 在本次 snapshot 文本中的 0-based 行号。
    pub line_index: u32,
    /// 匹配在该行中的 0-based 字符偏移。
    pub column_index: u16,
    pub line_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchResultPayload {
    pub session_id: SessionId,
    pub query: String,
    pub line_count: u32,
    pub matches: Vec<SessionSearchMatchPayload>,
    pub truncated: bool,
}

/// 文件列表条目的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFileKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// 查询某个 session 关联的文件目录。
///
/// `path` 为空时使用 session 启动目录；传入绝对路径或相对路径时，daemon 只依赖本机 OS
/// 权限判断可访问性，不在协议里引入账号/RBAC。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilesPayload {
    pub session_id: SessionId,
    #[serde(default)]
    pub path: Option<String>,
}

/// 单个文件或目录的只读展示信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileEntryPayload {
    pub name: String,
    pub path: String,
    pub kind: SessionFileKind,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilesResultPayload {
    pub session_id: SessionId,
    pub path: String,
    pub entries: Vec<SessionFileEntryPayload>,
}

/// 查询某个 session 当前终端目录关联的 Git 状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitPayload {
    pub session_id: SessionId,
}

/// Git 文件变更展示条目；`status` 保留 porcelain 的两列状态码，便于客户端直接展示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitFileChangePayload {
    pub path: String,
    pub status: String,
}

/// 一个 Git worktree 下的未提交文件分组。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitWorktreePayload {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_current: bool,
    pub staged: Vec<SessionGitFileChangePayload>,
    pub unstaged: Vec<SessionGitFileChangePayload>,
}

/// Git tab 的只读快照。`repository_root` 为空表示当前 cwd 不在 Git 仓库中。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitResultPayload {
    pub session_id: SessionId,
    pub cwd: String,
    pub repository_root: Option<String>,
    pub worktrees: Vec<SessionGitWorktreePayload>,
    pub graph: Vec<String>,
    pub error: Option<String>,
}

/// Git 文件操作只允许作用于 Git tab 已展示的 worktree 和相对路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionGitActionKind {
    Stage,
    Unstage,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitActionPayload {
    pub session_id: SessionId,
    pub worktree_path: String,
    pub file_path: String,
    pub action: SessionGitActionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitActionResultPayload {
    pub session_id: SessionId,
    pub worktree_path: String,
    pub file_path: String,
    pub action: SessionGitActionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitDiffPayload {
    pub session_id: SessionId,
    pub worktree_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default)]
    pub staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGitDiffResultPayload {
    pub session_id: SessionId,
    pub worktree_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub staged: bool,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileReadPayload {
    pub session_id: SessionId,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileReadResultPayload {
    pub session_id: SessionId,
    pub path: String,
    pub data_base64: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileWritePayload {
    pub session_id: SessionId,
    pub path: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileWrittenPayload {
    pub session_id: SessionId,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDeletePayload {
    pub session_id: SessionId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDeletedPayload {
    pub session_id: SessionId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadPreparePayload {
    pub session_id: SessionId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadReadyPayload {
    pub session_id: SessionId,
    pub path: String,
    pub token: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
    pub expires_at_ms: UnixTimestampMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadChunkPayload {
    pub session_id: SessionId,
    pub path: String,
    pub offset_bytes: u64,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadChunkResultPayload {
    pub session_id: SessionId,
    pub path: String,
    pub offset_bytes: u64,
    pub data_base64: String,
    pub next_offset_bytes: u64,
    pub size_bytes: u64,
    pub eof: bool,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileUploadPayload {
    pub session_id: SessionId,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileUploadReadyPayload {
    pub session_id: SessionId,
    pub path: String,
    pub size_bytes: u64,
    pub offset_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileHttpUploadReadyPayload {
    pub session_id: SessionId,
    pub path: String,
    pub upload_id: String,
    pub size_bytes: u64,
    pub offset_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileHttpUploadStreamPayload {
    pub session_id: SessionId,
    pub path: String,
    pub upload_id: String,
    pub size_bytes: u64,
    pub offset_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileHttpDownloadPayload {
    pub session_id: SessionId,
    pub path: String,
    #[serde(default)]
    pub offset_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileUploadProgressPayload {
    pub session_id: SessionId,
    pub path: String,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub eof: bool,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadStreamPayload {
    pub session_id: SessionId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileDownloadStreamReadyPayload {
    pub session_id: SessionId,
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileTransferChunkPayload {
    pub session_id: SessionId,
    pub offset_bytes: u64,
    pub data_base64: String,
    pub size_bytes: u64,
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequestPayload {
    pub session_id: SessionId,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlGrantPayload {
    pub session_id: SessionId,
    pub device_id: DeviceId,
}

/// 协议错误只返回稳定 code 和脱敏 message，不携带 token、签名、明文终端内容或 backtrace。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

/// ping/pong 可以携带 nonce 与时间戳，便于后续检测重放或乱序响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingPayload {
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PongPayload {
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
}

/// relay 分配的短期 client 连接 id。
///
/// 这个 id 只在 relay 与 daemon outbound connector 之间使用，不是设备身份，也不表达控制权。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelayClientId(pub u64);

/// relay transport 生命周期消息。
///
/// 该消息只用于建立/关闭/分配 relay transport 数据管道，不包含 terminal、session、
/// auth 明文。真正的 browser-daemon 业务流仍只在配对后的 data 线上按原始
/// WebSocket frame 透传。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayControlEnvelope {
    OpenData {
        client_id: RelayClientId,
        data_token: Nonce,
        #[serde(default)]
        route_kind: RelayRouteKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_token: Option<String>,
    },
    ClientDisconnected {
        client_id: RelayClientId,
    },
    /// daemon data pipe 已完成旧 client 上下文清理，可以重新进入 relay idle 池。
    DataReady,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayRouteKind {
    #[default]
    Legacy,
    Metadata,
    Terminal,
    Http,
}

const RELAY_DATA_CONTROL_MAGIC: &[u8] = b"tdc1";

/// 把 relay data 线生命周期控制编码进 WebSocket control frame payload。
///
/// data 线的 text/binary frame 必须保持业务原样透传；因此不能再用普通 JSON text
/// 承载 `data_ready` / `client_disconnected` 等 transport 控制消息，否则会和合法业务
/// text frame 的 JSON 形状发生碰撞。这里使用 WebSocket ping/pong payload 中的紧凑二进制
/// 控制格式，避开业务 text/binary 命名空间，并保持 payload 小于控制帧 125 字节限制。
pub fn encode_relay_data_control(envelope: &RelayControlEnvelope) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(RELAY_DATA_CONTROL_MAGIC);
    match envelope {
        RelayControlEnvelope::OpenData {
            client_id,
            data_token,
            ..
        } => {
            let token = data_token.0.as_bytes();
            let token_len = u8::try_from(token.len()).ok()?;
            out.push(1);
            out.extend_from_slice(&client_id.0.to_be_bytes());
            out.push(token_len);
            out.extend_from_slice(token);
        }
        RelayControlEnvelope::ClientDisconnected { client_id } => {
            out.push(2);
            out.extend_from_slice(&client_id.0.to_be_bytes());
        }
        RelayControlEnvelope::DataReady => {
            out.push(3);
        }
    }
    (out.len() <= 125).then_some(out)
}

/// 解码 relay data 线 WebSocket control frame payload。
pub fn decode_relay_data_control(payload: &[u8]) -> Option<RelayControlEnvelope> {
    let rest = payload.strip_prefix(RELAY_DATA_CONTROL_MAGIC)?;
    let (&kind, rest) = rest.split_first()?;
    match kind {
        1 => {
            if rest.len() < 9 {
                return None;
            }
            let mut client_id = [0_u8; 8];
            client_id.copy_from_slice(&rest[..8]);
            let token_len = rest[8] as usize;
            let token = rest.get(9..9 + token_len)?;
            if rest.len() != 9 + token_len {
                return None;
            }
            let data_token = String::from_utf8(token.to_vec()).ok()?;
            Some(RelayControlEnvelope::OpenData {
                client_id: RelayClientId(u64::from_be_bytes(client_id)),
                data_token: Nonce(data_token),
                route_kind: RelayRouteKind::Legacy,
                access_token: None,
            })
        }
        2 => {
            if rest.len() != 8 {
                return None;
            }
            let mut client_id = [0_u8; 8];
            client_id.copy_from_slice(rest);
            Some(RelayControlEnvelope::ClientDisconnected {
                client_id: RelayClientId(u64::from_be_bytes(client_id)),
            })
        }
        3 => rest.is_empty().then_some(RelayControlEnvelope::DataReady),
        _ => None,
    }
}

/// relay mux 通道承载的不透明 WebSocket frame。
///
/// relay 只知道 frame 是 text 还是 binary；binary 通过 base64 放进 JSON transport envelope。
/// 业务层的 `hello`、`pair_request`、`session_data` 等内容仍由 daemon/client 解释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayOpaqueFrame {
    Text { data: String },
    Binary { data_base64: String },
}

/// relay 与 daemon outbound connector 之间的多路复用 transport envelope。
///
/// 该 envelope 只解决“一条 daemon relay socket 服务多个 relay client socket”的寻址问题。
/// 它不包含鉴权、session、operator 状态或任何业务判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMuxEnvelope {
    Keepalive {
        nonce: u64,
    },
    KeepaliveAck {
        nonce: u64,
    },
    ClientConnected {
        client_id: RelayClientId,
    },
    ClientDisconnected {
        client_id: RelayClientId,
    },
    ClientFrame {
        client_id: RelayClientId,
        frame: RelayOpaqueFrame,
    },
    DaemonFrame {
        client_id: RelayClientId,
        frame: RelayOpaqueFrame,
    },
}

const BINARY_RELAY_MUX_MAGIC: &[u8; 4] = b"TD2M";
const BINARY_RELAY_MUX_VERSION: u8 = 1;
const BINARY_RELAY_MUX_HEADER_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryRelayMuxKind {
    ClientConnected = 1,
    ClientDisconnected = 2,
    ClientTextFrame = 3,
    ClientBinaryFrame = 4,
    DaemonTextFrame = 5,
    DaemonBinaryFrame = 6,
    Keepalive = 7,
    KeepaliveAck = 8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryRelayMuxFrameError {
    InvalidFrame,
    InvalidBase64,
    InvalidUtf8,
}

pub fn encode_binary_relay_mux_envelope(
    envelope: &RelayMuxEnvelope,
) -> Result<Vec<u8>, BinaryRelayMuxFrameError> {
    let (kind, client_id, payload) = match envelope {
        RelayMuxEnvelope::Keepalive { nonce } => (
            BinaryRelayMuxKind::Keepalive,
            RelayClientId(*nonce),
            Vec::new(),
        ),
        RelayMuxEnvelope::KeepaliveAck { nonce } => (
            BinaryRelayMuxKind::KeepaliveAck,
            RelayClientId(*nonce),
            Vec::new(),
        ),
        RelayMuxEnvelope::ClientConnected { client_id } => {
            (BinaryRelayMuxKind::ClientConnected, *client_id, Vec::new())
        }
        RelayMuxEnvelope::ClientDisconnected { client_id } => (
            BinaryRelayMuxKind::ClientDisconnected,
            *client_id,
            Vec::new(),
        ),
        RelayMuxEnvelope::ClientFrame { client_id, frame } => {
            let (kind, payload) = binary_relay_mux_payload(
                frame,
                BinaryRelayMuxKind::ClientTextFrame,
                BinaryRelayMuxKind::ClientBinaryFrame,
            )?;
            (kind, *client_id, payload)
        }
        RelayMuxEnvelope::DaemonFrame { client_id, frame } => {
            let (kind, payload) = binary_relay_mux_payload(
                frame,
                BinaryRelayMuxKind::DaemonTextFrame,
                BinaryRelayMuxKind::DaemonBinaryFrame,
            )?;
            (kind, *client_id, payload)
        }
    };
    let mut wire = Vec::with_capacity(BINARY_RELAY_MUX_HEADER_LEN + payload.len());
    wire.extend_from_slice(BINARY_RELAY_MUX_MAGIC);
    wire.push(BINARY_RELAY_MUX_VERSION);
    wire.push(kind as u8);
    wire.extend_from_slice(&[0, 0]);
    wire.extend_from_slice(&client_id.0.to_be_bytes());
    wire.extend_from_slice(&payload);
    Ok(wire)
}

pub fn decode_binary_relay_mux_envelope(
    wire: &[u8],
) -> Result<RelayMuxEnvelope, BinaryRelayMuxFrameError> {
    if wire.len() < BINARY_RELAY_MUX_HEADER_LEN
        || &wire[..4] != BINARY_RELAY_MUX_MAGIC
        || wire[4] != BINARY_RELAY_MUX_VERSION
        || wire[6] != 0
        || wire[7] != 0
    {
        return Err(BinaryRelayMuxFrameError::InvalidFrame);
    }
    let kind = binary_relay_mux_kind(wire[5])?;
    let client_id = RelayClientId(u64::from_be_bytes(
        wire[8..16]
            .try_into()
            .map_err(|_| BinaryRelayMuxFrameError::InvalidFrame)?,
    ));
    let payload = &wire[BINARY_RELAY_MUX_HEADER_LEN..];
    match kind {
        BinaryRelayMuxKind::Keepalive => Ok(RelayMuxEnvelope::Keepalive { nonce: client_id.0 }),
        BinaryRelayMuxKind::KeepaliveAck => {
            Ok(RelayMuxEnvelope::KeepaliveAck { nonce: client_id.0 })
        }
        BinaryRelayMuxKind::ClientConnected => Ok(RelayMuxEnvelope::ClientConnected { client_id }),
        BinaryRelayMuxKind::ClientDisconnected => {
            Ok(RelayMuxEnvelope::ClientDisconnected { client_id })
        }
        BinaryRelayMuxKind::ClientTextFrame => Ok(RelayMuxEnvelope::ClientFrame {
            client_id,
            frame: RelayOpaqueFrame::Text {
                data: String::from_utf8(payload.to_vec())
                    .map_err(|_| BinaryRelayMuxFrameError::InvalidUtf8)?,
            },
        }),
        BinaryRelayMuxKind::ClientBinaryFrame => Ok(RelayMuxEnvelope::ClientFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: base64::engine::general_purpose::STANDARD.encode(payload),
            },
        }),
        BinaryRelayMuxKind::DaemonTextFrame => Ok(RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Text {
                data: String::from_utf8(payload.to_vec())
                    .map_err(|_| BinaryRelayMuxFrameError::InvalidUtf8)?,
            },
        }),
        BinaryRelayMuxKind::DaemonBinaryFrame => Ok(RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: base64::engine::general_purpose::STANDARD.encode(payload),
            },
        }),
    }
}

const RELAY_HTTP_TUNNEL_STREAM_MAGIC: &[u8; 4] = b"TDHT";
const RELAY_HTTP_TUNNEL_REQUEST_HEAD: u8 = 1;
const RELAY_HTTP_TUNNEL_REQUEST_BODY: u8 = 2;
const RELAY_HTTP_TUNNEL_REQUEST_END: u8 = 3;
const RELAY_HTTP_TUNNEL_RESPONSE_HEAD: u8 = 4;
const RELAY_HTTP_TUNNEL_RESPONSE_BODY: u8 = 5;
const RELAY_HTTP_TUNNEL_RESPONSE_END: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RelayHttpTunnelHead {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayHttpTunnelFrame {
    RequestHead {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
    },
    RequestBody {
        body: Vec<u8>,
    },
    RequestEnd,
    ResponseHead {
        status: u16,
    },
    ResponseBody {
        body: Vec<u8>,
    },
    ResponseEnd,
}

pub fn encode_relay_http_tunnel_request_head(
    method: String,
    path: String,
    headers: Vec<(String, String)>,
) -> Result<Vec<u8>, serde_json::Error> {
    let head = serde_json::to_vec(&RelayHttpTunnelHead {
        method,
        path,
        headers,
    })?;
    Ok(encode_relay_http_tunnel_stream_frame(
        RELAY_HTTP_TUNNEL_REQUEST_HEAD,
        &head,
    ))
}

pub fn encode_relay_http_tunnel_request_body(body: Vec<u8>) -> Vec<u8> {
    encode_relay_http_tunnel_stream_frame(RELAY_HTTP_TUNNEL_REQUEST_BODY, &body)
}

pub fn encode_relay_http_tunnel_request_end() -> Vec<u8> {
    encode_relay_http_tunnel_stream_frame(RELAY_HTTP_TUNNEL_REQUEST_END, &[])
}

pub fn encode_relay_http_tunnel_response_head(status: u16) -> Vec<u8> {
    encode_relay_http_tunnel_stream_frame(RELAY_HTTP_TUNNEL_RESPONSE_HEAD, &status.to_be_bytes())
}

pub fn encode_relay_http_tunnel_response_body(body: Vec<u8>) -> Vec<u8> {
    encode_relay_http_tunnel_stream_frame(RELAY_HTTP_TUNNEL_RESPONSE_BODY, &body)
}

pub fn encode_relay_http_tunnel_response_end() -> Vec<u8> {
    encode_relay_http_tunnel_stream_frame(RELAY_HTTP_TUNNEL_RESPONSE_END, &[])
}

pub fn decode_relay_http_tunnel_frame(raw: &[u8]) -> Option<RelayHttpTunnelFrame> {
    if raw.len() < 5 || &raw[0..4] != RELAY_HTTP_TUNNEL_STREAM_MAGIC {
        return None;
    }
    let kind = raw[4];
    let payload = &raw[5..];
    match kind {
        RELAY_HTTP_TUNNEL_REQUEST_HEAD => {
            let head: RelayHttpTunnelHead = serde_json::from_slice(payload).ok()?;
            Some(RelayHttpTunnelFrame::RequestHead {
                method: head.method,
                path: head.path,
                headers: head.headers,
            })
        }
        RELAY_HTTP_TUNNEL_REQUEST_BODY => Some(RelayHttpTunnelFrame::RequestBody {
            body: payload.to_vec(),
        }),
        RELAY_HTTP_TUNNEL_REQUEST_END if payload.is_empty() => {
            Some(RelayHttpTunnelFrame::RequestEnd)
        }
        RELAY_HTTP_TUNNEL_RESPONSE_HEAD if payload.len() == 2 => {
            Some(RelayHttpTunnelFrame::ResponseHead {
                status: u16::from_be_bytes(payload.try_into().ok()?),
            })
        }
        RELAY_HTTP_TUNNEL_RESPONSE_BODY => Some(RelayHttpTunnelFrame::ResponseBody {
            body: payload.to_vec(),
        }),
        RELAY_HTTP_TUNNEL_RESPONSE_END if payload.is_empty() => {
            Some(RelayHttpTunnelFrame::ResponseEnd)
        }
        _ => None,
    }
}

fn encode_relay_http_tunnel_stream_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(5 + payload.len());
    wire.extend_from_slice(RELAY_HTTP_TUNNEL_STREAM_MAGIC);
    wire.push(kind);
    wire.extend_from_slice(payload);
    wire
}

fn binary_relay_mux_payload(
    frame: &RelayOpaqueFrame,
    text_kind: BinaryRelayMuxKind,
    binary_kind: BinaryRelayMuxKind,
) -> Result<(BinaryRelayMuxKind, Vec<u8>), BinaryRelayMuxFrameError> {
    match frame {
        RelayOpaqueFrame::Text { data } => Ok((text_kind, data.as_bytes().to_vec())),
        RelayOpaqueFrame::Binary { data_base64 } => base64::engine::general_purpose::STANDARD
            .decode(data_base64)
            .map(|payload| (binary_kind, payload))
            .map_err(|_| BinaryRelayMuxFrameError::InvalidBase64),
    }
}

fn binary_relay_mux_kind(kind: u8) -> Result<BinaryRelayMuxKind, BinaryRelayMuxFrameError> {
    match kind {
        1 => Ok(BinaryRelayMuxKind::ClientConnected),
        2 => Ok(BinaryRelayMuxKind::ClientDisconnected),
        3 => Ok(BinaryRelayMuxKind::ClientTextFrame),
        4 => Ok(BinaryRelayMuxKind::ClientBinaryFrame),
        5 => Ok(BinaryRelayMuxKind::DaemonTextFrame),
        6 => Ok(BinaryRelayMuxKind::DaemonBinaryFrame),
        7 => Ok(BinaryRelayMuxKind::Keepalive),
        8 => Ok(BinaryRelayMuxKind::KeepaliveAck),
        _ => Err(BinaryRelayMuxFrameError::InvalidFrame),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v070_relay_open_data_carries_workspace_route_and_access_token() {
        let envelope = RelayControlEnvelope::OpenData {
            client_id: RelayClientId(7),
            data_token: Nonce("pipe-token".to_owned()),
            route_kind: RelayRouteKind::Terminal,
            access_token: Some("header.claims.signature".to_owned()),
        };

        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(value["route_kind"], "terminal");
        assert_eq!(value["access_token"], "header.claims.signature");
    }

    #[test]
    fn protocol_version_default_tracks_current_packet_version() {
        assert_eq!(ProtocolVersion::default().0, PROTOCOL_PACKET_VERSION);
    }

    #[test]
    fn all_message_types_use_snake_case_wire_names() {
        let cases = [
            (MessageType::RouteHello, "route_hello"),
            (MessageType::RouteReady, "route_ready"),
            (MessageType::Hello, "hello"),
            (MessageType::Auth, "auth"),
            (MessageType::AuthChallenge, "auth_challenge"),
            (MessageType::PairRequest, "pair_request"),
            (MessageType::PairAccept, "pair_accept"),
            (MessageType::SessionCreate, "session_create"),
            (MessageType::SessionCreated, "session_created"),
            (MessageType::SessionAttach, "session_attach"),
            (MessageType::SessionAttached, "session_attached"),
            (MessageType::SessionData, "session_data"),
            (MessageType::TerminalFrame, "terminal_frame"),
            (MessageType::AttachFrame, "attach_frame"),
            (MessageType::SessionActivity, "session_activity"),
            (MessageType::SessionCwdChanged, "session_cwd_changed"),
            (MessageType::SessionCursor, "session_cursor"),
            (MessageType::SessionResize, "session_resize"),
            (MessageType::SessionResized, "session_resized"),
            (MessageType::SessionRename, "session_rename"),
            (MessageType::SessionRenamed, "session_renamed"),
            (MessageType::SessionReorder, "session_reorder"),
            (MessageType::SessionReordered, "session_reordered"),
            (MessageType::SessionClose, "session_close"),
            (MessageType::SessionClosed, "session_closed"),
            (MessageType::SessionSearch, "session_search"),
            (MessageType::SessionSearchResult, "session_search_result"),
            (MessageType::SessionFiles, "session_files"),
            (MessageType::SessionFilesResult, "session_files_result"),
            (MessageType::SessionGit, "session_git"),
            (MessageType::SessionGitResult, "session_git_result"),
            (MessageType::SessionGitAction, "session_git_action"),
            (
                MessageType::SessionGitActionResult,
                "session_git_action_result",
            ),
            (MessageType::SessionGitDiff, "session_git_diff"),
            (MessageType::SessionGitDiffResult, "session_git_diff_result"),
            (MessageType::SessionFileRead, "session_file_read"),
            (
                MessageType::SessionFileReadResult,
                "session_file_read_result",
            ),
            (MessageType::SessionFileWrite, "session_file_write"),
            (MessageType::SessionFileWritten, "session_file_written"),
            (MessageType::SessionFileDelete, "session_file_delete"),
            (MessageType::SessionFileDeleted, "session_file_deleted"),
            (
                MessageType::SessionFileDownloadPrepare,
                "session_file_download_prepare",
            ),
            (
                MessageType::SessionFileDownloadReady,
                "session_file_download_ready",
            ),
            (
                MessageType::SessionFileDownloadChunk,
                "session_file_download_chunk",
            ),
            (
                MessageType::SessionFileDownloadChunkResult,
                "session_file_download_chunk_result",
            ),
            (MessageType::SessionList, "session_list"),
            (MessageType::SessionListResult, "session_list_result"),
            (MessageType::ClientHello, "client_hello"),
            (MessageType::DaemonClients, "daemon_clients"),
            (MessageType::DaemonClientsResult, "daemon_clients_result"),
            (MessageType::DaemonClientForget, "daemon_client_forget"),
            (MessageType::DaemonClientForgot, "daemon_client_forgot"),
            (MessageType::DaemonStatus, "daemon_status"),
            (MessageType::DaemonStatusResult, "daemon_status_result"),
            (MessageType::MetadataSubscribe, "metadata_subscribe"),
            (
                MessageType::DaemonClientsSnapshot,
                "daemon_clients_snapshot",
            ),
            (MessageType::DaemonStatusSnapshot, "daemon_status_snapshot"),
            (MessageType::ControlRequest, "control_request"),
            (MessageType::ControlGrant, "control_grant"),
            (MessageType::Error, "error"),
            (MessageType::Ping, "ping"),
            (MessageType::Pong, "pong"),
        ];

        for (kind, expected) in cases {
            let json = serde_json::to_value(kind).expect("message type should serialize");
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn state_enums_use_documented_wire_format() {
        assert_eq!(
            serde_json::to_value(SessionState::Created).unwrap(),
            "created"
        );
        assert_eq!(
            serde_json::to_value(SessionState::Running).unwrap(),
            "running"
        );
        assert_eq!(
            serde_json::to_value(SessionState::Closed).unwrap(),
            "closed"
        );
        assert_eq!(serde_json::to_value(ConnectionState::Init).unwrap(), "init");
        assert_eq!(serde_json::to_value(ConnectionState::Auth).unwrap(), "auth");
        assert_eq!(
            serde_json::to_value(ConnectionState::Attached).unwrap(),
            "attached"
        );
        assert_eq!(
            serde_json::to_value(ConnectionState::Closed).unwrap(),
            "closed"
        );
        assert_eq!(
            serde_json::to_value(AttachRole::Operator).unwrap(),
            "operator"
        );
        assert_eq!(serde_json::to_value(RouteRole::Client).unwrap(), "client");
        assert_eq!(
            serde_json::to_value(RouteRole::DaemonMux).unwrap(),
            "daemon_mux"
        );
    }

    #[test]
    fn control_state_serializes_without_permission_model() {
        let device_id = DeviceId(Uuid::nil());
        let held = serde_json::to_value(ControlState::Held { device_id }).unwrap();

        assert_eq!(
            serde_json::to_value(ControlState::None).unwrap()["state"],
            "none"
        );
        assert_eq!(held["state"], "held");
        assert_eq!(held["device_id"], Uuid::nil().to_string());
    }

    #[test]
    fn envelope_serializes_with_type_field() {
        let envelope = Envelope::new(
            MessageType::SessionResize,
            SessionResizePayload {
                session_id: SessionId(Uuid::nil()),
                size: TerminalSize::new(40, 120),
            },
        );

        let json = serde_json::to_value(envelope).expect("envelope should serialize");

        assert_eq!(json["type"], "session_resize");
        assert_eq!(json["payload"]["size"]["rows"], 40);
        assert_eq!(json["payload"]["size"]["cols"], 120);
    }

    #[test]
    fn route_prelude_payloads_roundtrip_without_secrets() {
        let server_id = ServerId::new();
        let route_hello = Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role: RouteRole::Client,
                protocol_version: ProtocolVersion::default(),
                nonce: Nonce("route-nonce".to_owned()),
                admission: None,
                route_generation: None,
                client_id: None,
                data_token: None,
                timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            },
        );
        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id,
                role: RouteRole::Client,
            },
        );
        let json = serde_json::to_value(&route_hello).unwrap();
        let raw = json.to_string();

        assert_eq!(json["type"], "route_hello");
        assert_eq!(json["payload"]["role"], "client");
        assert!(json["payload"].get("token").is_none());
        assert!(!raw.contains("pair"));
        assert_roundtrip(route_hello);
        assert_roundtrip(route_ready);
    }

    #[test]
    fn route_prelude_carries_trusted_relay_admission() {
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let route_hello = Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role: RouteRole::Client,
                protocol_version: ProtocolVersion::default(),
                nonce: Nonce("route-nonce".to_owned()),
                admission: Some(RelayAdmissionPayload::Device {
                    device_id,
                    nonce: Nonce("device-nonce".to_owned()),
                    timestamp_ms: UnixTimestampMillis(1_710_000_000_001),
                    signature: Signature("device-signature".to_owned()),
                }),
                route_generation: None,
                client_id: None,
                data_token: None,
                timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            },
        );

        let json = serde_json::to_value(&route_hello).unwrap();

        assert_eq!(json["payload"]["admission"]["kind"], "device");
        assert_eq!(
            json["payload"]["admission"]["device_id"],
            device_id.0.to_string()
        );
        assert_roundtrip(route_hello);
    }

    #[test]
    fn mvp_auth_and_pairing_payloads_roundtrip() {
        let device_id = DeviceId::new();
        let server_id = ServerId::new();
        let hello = HelloPayload {
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce("hello-nonce".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            server_id: Some(server_id),
            daemon_public_key: Some(PublicKey("daemon-pub".to_owned())),
            device_id: Some(device_id),
        };
        let auth = AuthPayload {
            device_id,
            challenge: Challenge("challenge".to_owned()),
            nonce: Nonce("auth-nonce".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_001),
            signature: Signature("signature".to_owned()),
        };
        let auth_challenge = AuthChallengePayload {
            device_id,
            challenge: Challenge("challenge".to_owned()),
            expires_at_ms: UnixTimestampMillis(1_710_000_060_000),
        };
        let pair_request = PairRequestPayload {
            device_id,
            device_public_key: PublicKey("device-pub".to_owned()),
            token: PairingToken("pair-token".to_owned()),
            nonce: Nonce("pair-nonce".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_002),
        };
        let pair_accept = PairAcceptPayload {
            server_id,
            daemon_public_key: PublicKey("daemon-pub".to_owned()),
            device_id,
            expires_at_ms: UnixTimestampMillis(1_710_000_060_000),
        };
        let qr_payload = PairingQrPayload::new(
            PairingToken("pair-token".to_owned()),
            server_id,
            UnixTimestampMillis(1_710_000_060_000),
        );

        assert_eq!(
            hello.protocol_version,
            ProtocolVersion(PROTOCOL_PACKET_VERSION)
        );
        assert_roundtrip(hello);
        assert_roundtrip(auth);
        assert_roundtrip(auth_challenge);
        assert_roundtrip(pair_request);
        assert_roundtrip(pair_accept);
        assert_roundtrip(qr_payload);
    }

    #[test]
    fn http_tunnel_allowlist_accepts_only_current_control_and_file_routes() {
        let session_id = SessionId::new();

        for path in [
            "/api/control/session/reorder".to_owned(),
            "/api/control/daemon/client_forget".to_owned(),
            format!("/api/control/session/{}/rename", session_id.0),
            format!("/api/control/session/{}/control", session_id.0),
            format!("/api/control/session/{}/close", session_id.0),
            format!("/api/control/session/{}/files", session_id.0),
            format!("/api/control/session/{}/search", session_id.0),
            format!("/api/control/session/{}/git", session_id.0),
            format!("/api/control/session/{}/git_diff", session_id.0),
            format!("/api/control/session/{}/git_action", session_id.0),
            format!("/api/control/session/{}/file_read", session_id.0),
            format!("/api/control/session/{}/file_write", session_id.0),
            format!("/api/control/session/{}/file_delete", session_id.0),
        ] {
            assert!(is_http_tunnel_path_allowed("POST", &path), "{path}");
        }

        for (method, path) in [
            ("POST", "/api/files/uploads".to_owned()),
            ("PUT", "/api/files/uploads/upload-id/chunks".to_owned()),
            ("POST", "/api/files/uploads/upload-id/commit".to_owned()),
            ("POST", "/api/files/uploads/upload-id/abort".to_owned()),
            ("POST", "/api/files/downloads".to_owned()),
            ("GET", "/api/files/downloads/download-id".to_owned()),
        ] {
            assert!(
                is_http_tunnel_path_allowed(method, &path),
                "{method} {path}"
            );
        }

        for path in [
            "/healthz",
            "/api/control/auth/verify",
            "/api/control/session/list",
            "/api/control/daemon/clients",
            "/api/control/daemon/status",
            "/api/control/session/attach",
            &format!("/api/control/session/{}/cursor", session_id.0),
            &format!("/api/control/session/{}/resize", session_id.0),
            &format!(
                "/api/control/session/{}/file_download_prepare",
                session_id.0
            ),
            &format!("/api/control/session/{}/file_download_chunk", session_id.0),
            "/api/control/session/not-a-uuid/files",
            "/api/control/session/list/extra",
            "/api/files/upload/init",
            "/api/files/upload",
            "/api/files/upload/abort",
            "/api/files/download",
            "/api/files/download/extra",
        ] {
            assert!(!is_http_tunnel_path_allowed("POST", path), "{path}");
        }

        assert!(!is_http_tunnel_path_allowed(
            "GET",
            "/api/files/downloads/download-id/extra"
        ));
        assert!(!is_http_tunnel_path_allowed(
            "POST",
            "/api/files/uploads/upload-id/chunks"
        ));
    }

    #[test]
    fn http_tunnel_allowlist_includes_device_migration_bootstrap_routes() {
        for path in [
            "/api/auth/device-certificate/migrate/challenge",
            "/api/auth/device-certificate/migrate",
        ] {
            assert!(is_http_tunnel_path_allowed("POST", path), "{path}");
            assert!(!is_http_tunnel_path_allowed("GET", path), "{path}");
        }
    }

    #[test]
    fn pairing_qr_payload_contains_only_pairing_route_and_token_material() {
        let payload = PairingQrPayload::new(
            PairingToken("pair-token".to_owned()),
            ServerId(Uuid::nil()),
            UnixTimestampMillis(1_710_000_060_000),
        );
        let json = serde_json::to_value(&payload).unwrap();
        let raw = json.to_string();

        assert_eq!(json["type"], PairingQrPayload::PAYLOAD_TYPE);
        assert_eq!(json["version"], PairingQrPayload::VERSION);
        assert_eq!(json["token"], "pair-token");
        assert!(json.get("ws_url").is_none());
        assert!(payload.is_supported_version());
        for forbidden in ["private", "session_data", "controller", "viewer", "rbac"] {
            assert!(!raw.contains(forbidden));
        }
    }

    #[test]
    fn pairing_qr_payload_invite_code_roundtrips() {
        let payload = PairingQrPayload::new(
            PairingToken("pair-token".to_owned()),
            ServerId(Uuid::nil()),
            UnixTimestampMillis(1_710_000_060_000),
        );
        let invite = payload.to_invite_code();

        assert!(invite.starts_with("termd-pair:v2:"));
        assert_eq!(PairingQrPayload::parse_invite_code(&invite), Some(payload));
    }

    #[test]
    fn pairing_qr_payload_accepts_legacy_v1_invite_code() {
        let payload = PairingQrPayload::new(
            PairingToken("pair-token".to_owned()),
            ServerId(Uuid::nil()),
            UnixTimestampMillis(1_710_000_060_000),
        )
        .with_daemon_public_key(PublicKey("ed25519-v1:legacy-daemon".to_owned()));
        let mut legacy = payload.clone();
        legacy.version = 1;
        let raw = serde_json::to_vec(&legacy).unwrap();
        let invite = format!("termd-pair:v1:{}", URL_SAFE_NO_PAD.encode(raw));

        assert_eq!(PairingQrPayload::parse_invite_code(&invite), Some(legacy));
    }

    #[test]
    fn pairing_qr_payload_uses_plaintext_trusted_relay_version() {
        let payload = PairingQrPayload::new(
            PairingToken("pair-token".to_owned()),
            ServerId(Uuid::nil()),
            UnixTimestampMillis(1_710_000_060_000),
        );
        let invite = payload.to_invite_code();
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(PairingQrPayload::VERSION, 2);
        assert!(invite.starts_with("termd-pair:v2:"));
        assert!(json.get("daemon_public_key").is_none());
    }

    #[test]
    fn session_and_control_payloads_roundtrip() {
        let session_id = SessionId::new();
        let device_id = DeviceId::new();
        let size = TerminalSize::new(32, 100);

        assert_roundtrip(SessionCreatePayload {
            command: vec!["/bin/sh".to_owned()],
            size,
        });
        assert_roundtrip(SessionCreatedPayload {
            session_id,
            name: Some("Ada".to_owned()),
            role: AttachRole::Operator,
            state: SessionState::Running,
            size,
            resize_owner: true,
        });
        assert_roundtrip(SessionAttachPayload {
            session_id,
            watch_updates: true,
            last_terminal_seq: Some(41),
        });
        assert_roundtrip(SessionAttachedPayload {
            session_id,
            role: AttachRole::Operator,
            state: SessionState::Running,
            size,
            resize_owner: false,
        });
        assert_roundtrip(SessionDataPayload {
            session_id,
            data_base64: "aGVsbG8=".to_owned(),
        });
        assert_roundtrip(AttachFramePayload {
            session_id,
            data_base64: "YXR0YWNoLWZyYW1l".to_owned(),
        });
        assert_roundtrip(TerminalFramePayload::Snapshot {
            session_id,
            base_seq: 1024,
            size,
            data_base64: "c25hcHNob3Q=".to_owned(),
        });
        assert_roundtrip(TerminalFramePayload::Output {
            session_id,
            terminal_seq: 1025,
            data_base64: "b3V0cHV0".to_owned(),
        });
        assert_roundtrip(TerminalFramePayload::Resize {
            session_id,
            terminal_seq: 1026,
            size,
        });
        assert_roundtrip(TerminalFramePayload::Exit {
            session_id,
            terminal_seq: 1027,
            code: Some(0),
        });
        assert_roundtrip(TerminalFramePayload::Batch {
            session_id,
            frames: vec![TerminalFramePayload::Output {
                session_id,
                terminal_seq: 1028,
                data_base64: "YmF0Y2g=".to_owned(),
            }],
        });
        assert_roundtrip(SessionActivityPayload {
            session_id,
            timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
        });
        assert_roundtrip(SessionCwdChangedPayload {
            session_id,
            cwd: "/tmp/work".to_owned(),
        });
        assert_roundtrip(SessionCursorPayload {
            session_id,
            row: 12,
            col: 8,
            focused: true,
        });
        assert_roundtrip(SessionResizePayload { session_id, size });
        assert_roundtrip(SessionResizedPayload {
            session_id,
            size,
            resize_owner: true,
        });
        assert_roundtrip(SessionRenamePayload {
            session_id,
            name: "work shell".to_owned(),
        });
        assert_roundtrip(SessionRenamedPayload {
            session_id,
            name: "work shell".to_owned(),
        });
        assert_roundtrip(SessionReorderPayload {
            session_ids: vec![session_id],
        });
        assert_roundtrip(SessionReorderedPayload {
            session_ids: vec![session_id],
        });
        assert_roundtrip(SessionClosePayload { session_id });
        assert_roundtrip(SessionClosedPayload { session_id });
        assert_roundtrip(SessionFilesPayload {
            session_id,
            path: Some("src".to_owned()),
        });
        assert_roundtrip(SessionFilesResultPayload {
            session_id,
            path: "src".to_owned(),
            entries: vec![
                SessionFileEntryPayload {
                    name: "bin".to_owned(),
                    path: "src/bin".to_owned(),
                    kind: SessionFileKind::Directory,
                    size_bytes: 0,
                    modified_at_ms: Some(UnixTimestampMillis(1_710_000_000_000)),
                },
                SessionFileEntryPayload {
                    name: "main.rs".to_owned(),
                    path: "src/main.rs".to_owned(),
                    kind: SessionFileKind::File,
                    size_bytes: 128,
                    modified_at_ms: None,
                },
            ],
        });
        assert_roundtrip(SessionGitPayload { session_id });
        assert_roundtrip(SessionGitResultPayload {
            session_id,
            cwd: "/repo".to_owned(),
            repository_root: Some("/repo".to_owned()),
            worktrees: vec![SessionGitWorktreePayload {
                path: "/repo".to_owned(),
                branch: Some("main".to_owned()),
                head: Some("a1b2c3d".to_owned()),
                is_current: true,
                staged: vec![SessionGitFileChangePayload {
                    path: "src/lib.rs".to_owned(),
                    status: "M ".to_owned(),
                }],
                unstaged: vec![SessionGitFileChangePayload {
                    path: "README.md".to_owned(),
                    status: " M".to_owned(),
                }],
            }],
            graph: vec!["* a1b2c3d main commit".to_owned()],
            error: None,
        });
        assert_roundtrip(SessionGitActionPayload {
            session_id,
            worktree_path: "/repo".to_owned(),
            file_path: "src/lib.rs".to_owned(),
            action: SessionGitActionKind::Stage,
        });
        assert_roundtrip(SessionGitActionResultPayload {
            session_id,
            worktree_path: "/repo".to_owned(),
            file_path: "src/lib.rs".to_owned(),
            action: SessionGitActionKind::Unstage,
        });
        assert_roundtrip(SessionFileReadPayload {
            session_id,
            path: "src/main.rs".to_owned(),
            max_bytes: None,
        });
        assert_roundtrip(SessionFileReadPayload {
            session_id,
            path: "large.txt".to_owned(),
            max_bytes: Some(1024),
        });
        assert_roundtrip(SessionFileReadResultPayload {
            session_id,
            path: "src/main.rs".to_owned(),
            data_base64: "Zm4gbWFpbigpIHt9Cg==".to_owned(),
            size_bytes: 13,
            modified_at_ms: None,
        });
        assert_roundtrip(SessionFileWritePayload {
            session_id,
            path: "upload.txt".to_owned(),
            data_base64: "dXBsb2FkCg==".to_owned(),
        });
        assert_roundtrip(SessionFileWrittenPayload {
            session_id,
            path: "upload.txt".to_owned(),
            size_bytes: 7,
            modified_at_ms: Some(UnixTimestampMillis(1_710_000_000_000)),
        });
        assert_roundtrip(SessionFileDeletePayload {
            session_id,
            path: "upload.txt".to_owned(),
        });
        assert_roundtrip(SessionFileDeletedPayload {
            session_id,
            path: "upload.txt".to_owned(),
        });
        assert_roundtrip(SessionFileDownloadPreparePayload {
            session_id,
            path: "large.bin".to_owned(),
        });
        assert_roundtrip(SessionFileDownloadReadyPayload {
            session_id,
            path: "large.bin".to_owned(),
            token: "download-token".to_owned(),
            size_bytes: 4096,
            modified_at_ms: Some(UnixTimestampMillis(1_710_000_000_000)),
            expires_at_ms: UnixTimestampMillis(1_710_000_060_000),
        });
        assert_roundtrip(SessionFileDownloadChunkPayload {
            session_id,
            path: "large.bin".to_owned(),
            offset_bytes: 1024,
            max_bytes: 65_536,
        });
        assert_roundtrip(SessionFileDownloadChunkResultPayload {
            session_id,
            path: "large.bin".to_owned(),
            offset_bytes: 1024,
            data_base64: "Y2h1bms=".to_owned(),
            next_offset_bytes: 1029,
            size_bytes: 4096,
            eof: false,
            modified_at_ms: Some(UnixTimestampMillis(1_710_000_000_000)),
        });
        assert_roundtrip(SessionListPayload {});
        assert_roundtrip(SessionListResultPayload {
            sessions: vec![SessionSummaryPayload {
                session_id,
                name: Some("work shell".to_owned()),
                state: SessionState::Running,
                size,
                files_path: Some("/home/me/project".to_owned()),
                created_at_ms: Some(UnixTimestampMillis(1_710_000_000_000)),
            }],
        });
        assert_roundtrip(ControlRequestPayload {
            session_id,
            device_id,
        });
        assert_roundtrip(ControlGrantPayload {
            session_id,
            device_id,
        });
    }

    #[test]
    fn session_resize_owner_fields_default_for_older_payloads() {
        let session_id = SessionId::new();
        let size = TerminalSize::new(24, 80);
        let created: SessionCreatedPayload = serde_json::from_value(serde_json::json!({
            "session_id": session_id,
            "name": "Ada",
            "role": "operator",
            "state": "running",
            "size": size,
        }))
        .unwrap();
        let attached: SessionAttachedPayload = serde_json::from_value(serde_json::json!({
            "session_id": session_id,
            "role": "operator",
            "state": "running",
            "size": size,
        }))
        .unwrap();
        let resized: SessionResizedPayload = serde_json::from_value(serde_json::json!({
            "session_id": session_id,
            "size": size,
        }))
        .unwrap();

        assert!(!created.resize_owner);
        assert!(!attached.resize_owner);
        assert!(!resized.resize_owner);
    }

    #[test]
    fn daemon_client_payloads_roundtrip_with_online_state() {
        let session_id = SessionId::new();
        let device_id = DeviceId::new();
        let client_id = ClientId::new();

        assert_eq!(
            serde_json::to_value(MessageType::DaemonClients).unwrap(),
            "daemon_clients"
        );
        assert_eq!(
            serde_json::to_value(MessageType::DaemonClientsResult).unwrap(),
            "daemon_clients_result"
        );
        assert_eq!(
            serde_json::to_value(MessageType::DaemonStatus).unwrap(),
            "daemon_status"
        );
        assert_eq!(
            serde_json::to_value(MessageType::DaemonStatusResult).unwrap(),
            "daemon_status_result"
        );
        assert_eq!(
            serde_json::to_value(MessageType::MetadataSubscribe).unwrap(),
            "metadata_subscribe"
        );
        assert_eq!(
            serde_json::to_value(MessageType::DaemonClientsSnapshot).unwrap(),
            "daemon_clients_snapshot"
        );
        assert_eq!(
            serde_json::to_value(MessageType::DaemonStatusSnapshot).unwrap(),
            "daemon_status_snapshot"
        );
        assert_roundtrip(ClientHelloPayload {
            name: "Browser on Linux".to_owned(),
            kind: ClientHelloKind::Interactive,
        });
        let legacy_hello: ClientHelloPayload = serde_json::from_value(serde_json::json!({
            "name": "Legacy browser"
        }))
        .unwrap();
        assert_eq!(legacy_hello.kind, ClientHelloKind::Interactive);
        assert_roundtrip(ClientHelloPayload {
            name: "Metadata sidecar".to_owned(),
            kind: ClientHelloKind::Metadata,
        });
        assert_roundtrip(DaemonClientsPayload {});
        assert_roundtrip(MetadataSubscribePayload {
            status_interval_ms: Some(3_000),
            clients: true,
        });
        assert_roundtrip(DaemonClientsResultPayload {
            clients: vec![DaemonClientSummaryPayload {
                client_id,
                device_id,
                name: Some("Browser on Linux".to_owned()),
                peer_ip: Some("192.0.2.10".to_owned()),
                online: false,
                connected_at_ms: UnixTimestampMillis(1_710_000_000_000),
                last_seen_at_ms: UnixTimestampMillis(1_710_000_030_000),
                attached_session_ids: vec![session_id],
                cursor_session_id: Some(session_id),
                cursor_row: Some(12),
                cursor_col: Some(8),
                cursor_focused: Some(true),
            }],
        });
        assert_roundtrip(DaemonClientForgetPayload { device_id });
        assert_roundtrip(DaemonClientForgotPayload { device_id });
        assert_roundtrip(DaemonStatusPayload {});
        assert_roundtrip(DaemonStatusResultPayload {
            host_name: Some("devbox".to_owned()),
            load_avg: [0.1, 0.2, 0.3],
            uptime_seconds: 123,
            cpu_percent: 12.5,
            memory_total_bytes: 8 * 1024 * 1024,
            memory_available_bytes: 4 * 1024 * 1024,
            disk_total_bytes: 100 * 1024 * 1024,
            disk_available_bytes: 40 * 1024 * 1024,
            network_rx_bytes: 12 * 1024 * 1024,
            network_tx_bytes: 3 * 1024 * 1024,
            process_count: 42,
            atop_available: false,
        });
    }

    #[test]
    fn envelope_roundtrips_with_session_payload() {
        let envelope = Envelope::new(
            MessageType::SessionAttach,
            SessionAttachPayload {
                session_id: SessionId::new(),
                watch_updates: true,
                last_terminal_seq: None,
            },
        );

        assert_roundtrip(envelope);
    }

    #[test]
    fn session_attach_defaults_to_watching_updates_for_old_clients() {
        let session_id = SessionId::new();
        let payload: SessionAttachPayload =
            serde_json::from_value(serde_json::json!({ "session_id": session_id })).unwrap();

        assert!(payload.watch_updates);
    }

    #[test]
    fn error_payload_roundtrips_without_secret_fields() {
        let error = Envelope::new(
            MessageType::Error,
            ErrorPayload {
                code: "unauthenticated".to_owned(),
                message: "device must authenticate before session operations".to_owned(),
            },
        );

        let json = serde_json::to_value(&error).unwrap();

        assert_eq!(json["type"], "error");
        assert!(json["payload"].get("token").is_none());
        assert!(json["payload"].get("signature").is_none());
        assert_roundtrip(error);
    }

    #[test]
    fn ping_and_pong_payloads_roundtrip() {
        assert_roundtrip(PingPayload {
            nonce: Nonce("ping".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_003),
        });
        assert_roundtrip(PongPayload {
            nonce: Nonce("pong".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_004),
        });
    }

    #[test]
    fn removed_runtime_message_types_are_not_deserializable() {
        for wire_name in [
            "e2ee_key_exchange",
            "encrypted_frame",
            "session_scope_grant",
            "packet",
        ] {
            assert!(serde_json::from_value::<MessageType>(serde_json::json!(wire_name)).is_err());
        }
    }

    #[test]
    fn control_state_exposes_holder_without_permissions() {
        let device_id = DeviceId::new();
        let control = ControlState::Held { device_id };

        assert_eq!(control.holder(), Some(device_id));
        assert_eq!(ControlState::None.holder(), None);
    }

    #[test]
    fn relay_mux_payloads_roundtrip_without_business_semantics() {
        let client_id = RelayClientId(7);
        assert_roundtrip(RelayMuxEnvelope::Keepalive { nonce: 11 });
        assert_roundtrip(RelayMuxEnvelope::KeepaliveAck { nonce: 11 });
        assert_roundtrip(RelayMuxEnvelope::ClientConnected { client_id });
        assert_roundtrip(RelayMuxEnvelope::ClientDisconnected { client_id });
        assert_roundtrip(RelayMuxEnvelope::ClientFrame {
            client_id,
            frame: RelayOpaqueFrame::Text {
                data: "{\"type\":\"pair_request\",\"payload\":{\"token\":\"secret\"}}".to_owned(),
            },
        });
        assert_roundtrip(RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AQID".to_owned(),
            },
        });

        let json = serde_json::to_value(RelayMuxEnvelope::ClientConnected { client_id }).unwrap();
        assert_eq!(json["type"], "client_connected");
        assert_eq!(json["client_id"], 7);
        assert!(json.get("device_id").is_none());
        assert!(json.get("session_id").is_none());
        assert!(json.get("controller").is_none());
    }

    #[test]
    fn binary_relay_mux_frame_carries_opaque_binary_without_json_base64() {
        let client_id = RelayClientId(42);
        let envelope = RelayMuxEnvelope::ClientFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4]),
            },
        };

        let wire = encode_binary_relay_mux_envelope(&envelope).unwrap();
        let decoded = decode_binary_relay_mux_envelope(&wire).unwrap();

        assert_eq!(decoded, envelope);
        assert_eq!(&wire[..4], b"TD2M");
        assert_eq!(wire[5], 4);
        assert!(!String::from_utf8_lossy(&wire).contains("data_base64"));
        assert!(wire.ends_with(&[1, 2, 3, 4]));
    }

    fn assert_roundtrip<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("payload should serialize");
        let decoded = serde_json::from_str(&json).expect("payload should deserialize");

        assert_eq!(value, decoded);
    }
}
