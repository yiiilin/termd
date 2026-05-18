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
    SessionActivity,
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
    ControlRequest,
    ControlGrant,
    E2eeKeyExchange,
    EncryptedFrame,
    Packet,
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

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// 0.2.0 的加密业务包版本；外层 WebSocket/relay 仍只承担 transport。
pub const PROTOCOL_PACKET_VERSION: u16 = 3;

/// 一次 request/response 交互的关联 id。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PacketRequestId(pub Uuid);

impl PacketRequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PacketRequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// 流式交互的稳定 id；断线恢复和流控都围绕它表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PacketStreamId(pub Uuid);

impl PacketStreamId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PacketStreamId {
    fn default() -> Self {
        Self::new()
    }
}

/// 0.2.0 packet 的统一类型。请求、响应、事件、流和流控都用同一个外壳。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketKind {
    Request,
    Response,
    Event,
    StreamOpen,
    StreamChunk,
    StreamEnd,
    Cancel,
    Flow,
    Error,
}

/// packet 级错误必须绑定 request id 或 stream id，避免客户端猜测错误归属。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketErrorPayload {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

/// E2EE 内部承载的 0.2.0 业务 packet。
///
/// `id` 用于 unary request/response，`stream_id` 用于长流；`seq/ack/credit`
/// 是流式顺序、确认和背压字段。relay 不应解密或解释这些字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolPacket<P = serde_json::Value> {
    pub version: u16,
    pub kind: PacketKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<PacketRequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<PacketStreamId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<u32>,
    pub payload: P,
}

impl<P> ProtocolPacket<P> {
    fn new(kind: PacketKind, payload: P) -> Self {
        Self {
            version: PROTOCOL_PACKET_VERSION,
            kind,
            id: None,
            stream_id: None,
            method: None,
            seq: 0,
            ack: None,
            credit: None,
            payload,
        }
    }

    pub fn request(id: PacketRequestId, method: impl Into<String>, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::Request, payload);
        packet.id = Some(id);
        packet.method = Some(method.into());
        packet
    }

    pub fn response(id: PacketRequestId, method: impl Into<String>, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::Response, payload);
        packet.id = Some(id);
        packet.method = Some(method.into());
        packet
    }

    pub fn event(method: impl Into<String>, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::Event, payload);
        packet.method = Some(method.into());
        packet
    }

    pub fn stream_open(
        id: PacketRequestId,
        stream_id: PacketStreamId,
        method: impl Into<String>,
        credit: u32,
        payload: P,
    ) -> Self {
        let mut packet = Self::new(PacketKind::StreamOpen, payload);
        packet.id = Some(id);
        packet.stream_id = Some(stream_id);
        packet.method = Some(method.into());
        packet.credit = Some(credit);
        packet
    }

    pub fn stream_chunk(stream_id: PacketStreamId, seq: u64, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::StreamChunk, payload);
        packet.stream_id = Some(stream_id);
        packet.seq = seq;
        packet
    }

    pub fn stream_end(stream_id: PacketStreamId, seq: u64, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::StreamEnd, payload);
        packet.stream_id = Some(stream_id);
        packet.seq = seq;
        packet
    }

    pub fn cancel_request(id: PacketRequestId, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::Cancel, payload);
        packet.id = Some(id);
        packet
    }

    pub fn cancel_stream(stream_id: PacketStreamId, payload: P) -> Self {
        let mut packet = Self::new(PacketKind::Cancel, payload);
        packet.stream_id = Some(stream_id);
        packet
    }
}

impl ProtocolPacket<PacketErrorPayload> {
    pub fn request_error(id: PacketRequestId, payload: PacketErrorPayload) -> Self {
        let mut packet = Self::new(PacketKind::Error, payload);
        packet.id = Some(id);
        packet
    }

    pub fn stream_error(stream_id: PacketStreamId, payload: PacketErrorPayload) -> Self {
        let mut packet = Self::new(PacketKind::Error, payload);
        packet.stream_id = Some(stream_id);
        packet
    }
}

impl ProtocolPacket<serde_json::Value> {
    pub fn flow(stream_id: PacketStreamId, ack: u64, credit: u32) -> Self {
        let mut packet = Self::new(PacketKind::Flow, serde_json::json!({}));
        packet.stream_id = Some(stream_id);
        packet.ack = Some(ack);
        packet.credit = Some(credit);
        packet
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_public_key: Option<PublicKey>,
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
            daemon_public_key: None,
            expires_at_ms,
        }
    }

    pub fn with_daemon_public_key(mut self, daemon_public_key: PublicKey) -> Self {
        self.daemon_public_key = Some(daemon_public_key);
        self
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

impl E2eeKeyExchangePayload {
    pub fn new(
        server_id: ServerId,
        device_id: DeviceId,
        public_key: PublicKey,
        nonce: Nonce,
        timestamp_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            server_id,
            device_id,
            public_key,
            nonce,
            timestamp_ms,
            packet_version: None,
            signature: None,
        }
    }

    pub fn with_signature(mut self, signature: Signature) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn with_packet_version(mut self, packet_version: ProtocolVersion) -> Self {
        self.packet_version = Some(packet_version);
        self
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_version: Option<ProtocolVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
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

/// terminal stream 内的输出帧类型。
///
/// `ProtocolPacket.seq` 是连接内传输序号；这里的 `terminal_seq` / `base_seq`
/// 是 session 级终端事件序号，用于 snapshot 和 tail 的一致性判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFrameKind {
    Snapshot,
    Output,
    Resize,
    Exit,
}

/// packet terminal stream 的结构化帧。
///
/// snapshot 是替换语义，浏览器必须 reset xterm 后写入；output/resize/exit 是
/// `base_seq` 之后的增量 tail。不要把 snapshot 伪装成普通 `session_data`。
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
}

impl TerminalFramePayload {
    pub fn session_id(&self) -> SessionId {
        match self {
            Self::Snapshot { session_id, .. }
            | Self::Output { session_id, .. }
            | Self::Resize { session_id, .. }
            | Self::Exit { session_id, .. } => *session_id,
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
            (MessageType::TerminalFrame, "terminal_frame"),
            (MessageType::SessionActivity, "session_activity"),
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
            (MessageType::ControlRequest, "control_request"),
            (MessageType::ControlGrant, "control_grant"),
            (MessageType::E2eeKeyExchange, "e2ee_key_exchange"),
            (MessageType::EncryptedFrame, "encrypted_frame"),
            (MessageType::Packet, "packet"),
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
            E2eeKeyExchangePayload::new(
                server_id,
                device_id,
                PublicKey("x25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned()),
                Nonce("key-exchange-nonce".to_owned()),
                UnixTimestampMillis(1_710_000_000_005),
            ),
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

    #[test]
    fn protocol_packet_request_response_and_stream_shapes_are_stable() {
        let request_id = PacketRequestId::new();
        let stream_id = PacketStreamId::new();

        let request = ProtocolPacket::request(
            request_id,
            "session.list",
            serde_json::json!({"include_closed": false}),
        );
        let request_json = serde_json::to_value(&request).unwrap();
        assert_eq!(request_json.get("version"), Some(&serde_json::json!(3)));
        assert_eq!(
            request_json.get("kind"),
            Some(&serde_json::json!("request"))
        );
        assert_eq!(request_json.get("id"), Some(&serde_json::json!(request_id)));
        assert_eq!(
            request_json.get("method"),
            Some(&serde_json::json!("session.list"))
        );
        assert!(request_json.get("stream_id").is_none());

        let response = ProtocolPacket::response(
            request_id,
            "session.list",
            serde_json::json!({"sessions": []}),
        );
        let response_json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            response_json.get("kind"),
            Some(&serde_json::json!("response"))
        );
        assert_eq!(
            response_json.get("id"),
            Some(&serde_json::json!(request_id))
        );

        let chunk =
            ProtocolPacket::stream_chunk(stream_id, 7, serde_json::json!({"data_base64": "YWJj"}));
        let chunk_json = serde_json::to_value(&chunk).unwrap();
        assert_eq!(
            chunk_json.get("kind"),
            Some(&serde_json::json!("stream_chunk"))
        );
        assert_eq!(
            chunk_json.get("stream_id"),
            Some(&serde_json::json!(stream_id))
        );
        assert_eq!(chunk_json.get("seq"), Some(&serde_json::json!(7)));
        assert!(chunk_json.get("id").is_none());

        let flow = ProtocolPacket::flow(stream_id, 7, 64);
        let flow_json = serde_json::to_value(&flow).unwrap();
        assert_eq!(flow_json.get("kind"), Some(&serde_json::json!("flow")));
        assert_eq!(flow_json.get("ack"), Some(&serde_json::json!(7)));
        assert_eq!(flow_json.get("credit"), Some(&serde_json::json!(64)));
    }

    #[test]
    fn protocol_packet_error_is_bound_to_request_or_stream() {
        let request_id = PacketRequestId::new();
        let packet = ProtocolPacket::request_error(
            request_id,
            PacketErrorPayload {
                code: "timeout".to_owned(),
                message: "operation timed out".to_owned(),
                retryable: true,
            },
        );

        let json = serde_json::to_value(&packet).unwrap();
        assert_eq!(json.get("version"), Some(&serde_json::json!(3)));
        assert_eq!(json.get("kind"), Some(&serde_json::json!("error")));
        assert_eq!(json.get("id"), Some(&serde_json::json!(request_id)));
        assert!(json.get("stream_id").is_none());

        let decoded: ProtocolPacket<PacketErrorPayload> = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.kind, PacketKind::Error);
        assert_eq!(decoded.id, Some(request_id));
        assert_eq!(decoded.payload.code, "timeout");
        assert!(decoded.payload.retryable);
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
