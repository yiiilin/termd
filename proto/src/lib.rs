//! termd 的共享协议类型。
//!
//! 这个 crate 只描述客户端、daemon 与 relay 都需要知道的稳定外壳。
//! 具体业务规则仍由 daemon 执行，relay 只能基于外层路由字段转发密文。

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
    SessionActivity,
    SessionCursor,
    SessionResize,
    SessionResized,
    SessionRename,
    SessionRenamed,
    SessionClose,
    SessionClosed,
    SessionFiles,
    SessionFilesResult,
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
    ControlRequest,
    ControlGrant,
    E2eeKeyExchange,
    EncryptedFrame,
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
        Self(2)
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

/// pairing token 必须有过期时间；token 明文只允许出现在 E2EE 保护范围内。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairingToken(pub String);

/// 毫秒时间戳用于 replay protection 与 pairing token 过期判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTimestampMillis(pub u64);

/// WebSocket 建立后的明文路由角色。
///
/// 这里只表达连接方向：relay 据此把连接放进对应 server_id 的房间；daemon 直连只接受
/// `client`。它不是终端 operator / viewer 权限模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteRole {
    Client,
    DaemonMux,
}

/// WebSocket 第一帧路由前置握手。
///
/// 该消息只携带公开的 server_id 和连接方向，不携带 pairing token、session 数据或认证签名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteHelloPayload {
    pub server_id: ServerId,
    pub role: RouteRole,
    pub protocol_version: ProtocolVersion,
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
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

/// daemon 在 E2EE 内发送给已配对设备的短期认证挑战。
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
    pub expires_at_ms: UnixTimestampMillis,
}

impl PairingQrPayload {
    pub const PAYLOAD_TYPE: &'static str = "termd_pairing_qr";
    pub const VERSION: u16 = 1;
    const INVITE_PREFIX: &'static str = "termd-pair:v1:";

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
            expires_at_ms,
        }
    }

    pub fn is_supported_version(&self) -> bool {
        self.payload_type == Self::PAYLOAD_TYPE && self.version == Self::VERSION
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
        if let Some(encoded) = trimmed.strip_prefix(Self::INVITE_PREFIX) {
            let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
            let payload: Self = serde_json::from_slice(&bytes).ok()?;
            return payload.is_supported_version().then_some(payload);
        }

        let payload: Self = serde_json::from_str(trimmed).ok()?;
        payload.is_supported_version().then_some(payload)
    }
}

/// E2EE key exchange 只携带公开材料和防重放字段，不包含任何私钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eeKeyExchangePayload {
    pub server_id: ServerId,
    pub device_id: DeviceId,
    pub public_key: PublicKey,
    pub nonce: Nonce,
    pub timestamp_ms: UnixTimestampMillis,
}

/// 加密帧外层只暴露 relay 路由需要的信息，内部业务 envelope 必须整体放入密文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedFramePayload {
    pub server_id: ServerId,
    pub sequence: u64,
    pub ciphertext_base64: String,
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
}

/// attach 成功后的响应；shared-control 模式下 role 固定为 operator。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachedPayload {
    pub session_id: SessionId,
    pub role: AttachRole,
    pub state: SessionState,
    pub size: TerminalSize,
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
    /// xterm 侧上报的 1-based 行号，用于 Web 顶部 operator 列表展示。
    #[serde(default)]
    pub cursor_row: Option<u16>,
    /// xterm 侧上报的 1-based 列号，用于 Web 顶部 operator 列表展示。
    #[serde(default)]
    pub cursor_col: Option<u16>,
    /// xterm 是否处于聚焦状态；true 表示闪烁方块，false 表示非聚焦轮廓。
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
/// 该请求必须作为 E2EE 内层业务消息发送；relay 只能看到外层 encrypted_frame。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusPayload {}

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
    pub process_count: u64,
    pub atop_available: bool,
}

/// `session_data` 在 JSON 通道中使用 base64；二进制 WebSocket 帧可绕过这个结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDataPayload {
    pub session_id: SessionId,
    pub data_base64: String,
}

/// session 有新输出的轻量通知。
///
/// 该消息只用于 UI 标记后台 session，不携带终端明文，避免为了列表变色额外推送大块输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityPayload {
    pub session_id: SessionId,
    pub timestamp_ms: UnixTimestampMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachPayload {
    pub session_id: SessionId,
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
pub struct SessionClosePayload {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosedPayload {
    pub session_id: SessionId,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileReadPayload {
    pub session_id: SessionId,
    pub path: String,
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

#[cfg(test)]
mod tests {
    use super::*;

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
            (MessageType::SessionActivity, "session_activity"),
            (MessageType::SessionCursor, "session_cursor"),
            (MessageType::SessionResize, "session_resize"),
            (MessageType::SessionResized, "session_resized"),
            (MessageType::SessionRename, "session_rename"),
            (MessageType::SessionRenamed, "session_renamed"),
            (MessageType::SessionClose, "session_close"),
            (MessageType::SessionClosed, "session_closed"),
            (MessageType::SessionFiles, "session_files"),
            (MessageType::SessionFilesResult, "session_files_result"),
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
            (MessageType::ControlRequest, "control_request"),
            (MessageType::ControlGrant, "control_grant"),
            (MessageType::E2eeKeyExchange, "e2ee_key_exchange"),
            (MessageType::EncryptedFrame, "encrypted_frame"),
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
    fn mvp_auth_and_pairing_payloads_roundtrip() {
        let device_id = DeviceId::new();
        let server_id = ServerId::new();
        let hello = HelloPayload {
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce("hello-nonce".to_owned()),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            server_id: Some(server_id),
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

        assert_roundtrip(hello);
        assert_roundtrip(auth);
        assert_roundtrip(auth_challenge);
        assert_roundtrip(pair_request);
        assert_roundtrip(pair_accept);
        assert_roundtrip(qr_payload);
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

        assert!(invite.starts_with("termd-pair:v1:"));
        assert_eq!(PairingQrPayload::parse_invite_code(&invite), Some(payload));
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
        });
        assert_roundtrip(SessionAttachPayload { session_id });
        assert_roundtrip(SessionAttachedPayload {
            session_id,
            role: AttachRole::Operator,
            state: SessionState::Running,
            size,
        });
        assert_roundtrip(SessionDataPayload {
            session_id,
            data_base64: "aGVsbG8=".to_owned(),
        });
        assert_roundtrip(SessionActivityPayload {
            session_id,
            timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
        });
        assert_roundtrip(SessionCursorPayload {
            session_id,
            row: 12,
            col: 8,
            focused: true,
        });
        assert_roundtrip(SessionResizePayload { session_id, size });
        assert_roundtrip(SessionResizedPayload { session_id, size });
        assert_roundtrip(SessionRenamePayload {
            session_id,
            name: "work shell".to_owned(),
        });
        assert_roundtrip(SessionRenamedPayload {
            session_id,
            name: "work shell".to_owned(),
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
        assert_roundtrip(SessionFileReadPayload {
            session_id,
            path: "src/main.rs".to_owned(),
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
        assert_roundtrip(ClientHelloPayload {
            name: "Browser on Linux".to_owned(),
        });
        assert_roundtrip(DaemonClientsPayload {});
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
            },
        );

        assert_roundtrip(envelope);
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
    fn e2ee_message_types_use_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_value(MessageType::E2eeKeyExchange).unwrap(),
            "e2ee_key_exchange"
        );
        assert_eq!(
            serde_json::to_value(MessageType::EncryptedFrame).unwrap(),
            "encrypted_frame"
        );
    }

    #[test]
    fn e2ee_payloads_roundtrip_inside_unified_envelope() {
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let key_exchange = Envelope::new(
            MessageType::E2eeKeyExchange,
            E2eeKeyExchangePayload {
                server_id,
                device_id,
                public_key: PublicKey(
                    "x25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
                ),
                nonce: Nonce("key-exchange-nonce".to_owned()),
                timestamp_ms: UnixTimestampMillis(1_710_000_000_005),
            },
        );
        let encrypted_frame = Envelope::new(
            MessageType::EncryptedFrame,
            EncryptedFramePayload {
                server_id,
                sequence: 7,
                ciphertext_base64: "ciphertext".to_owned(),
            },
        );

        assert_roundtrip(key_exchange);
        assert_roundtrip(encrypted_frame);
    }

    #[test]
    fn e2ee_encrypted_frame_exposes_only_relay_routing_fields() {
        let frame = EncryptedFramePayload {
            server_id: ServerId::new(),
            sequence: 42,
            ciphertext_base64: "opaque-ciphertext".to_owned(),
        };

        let json = serde_json::to_value(frame).expect("encrypted frame should serialize");

        assert!(json.get("server_id").is_some());
        assert!(json.get("sequence").is_some());
        assert!(json.get("ciphertext_base64").is_some());
        assert!(json.get("session_id").is_none());
        assert!(json.get("data_base64").is_none());
        assert!(json.get("size").is_none());
        assert!(json.get("device_id").is_none());
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

    fn assert_roundtrip<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("payload should serialize");
        let decoded = serde_json::from_str(&json).expect("payload should deserialize");

        assert_eq!(value, decoded);
    }
}
