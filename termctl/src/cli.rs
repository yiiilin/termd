//! termctl 命令分发。
//!
//! CLI 默认面向人类输出：session/control/list 结果写 stdout，attach 的状态提示写 stderr，
//! 这样不会污染终端业务字节流。所有敏感输入只进入 E2EE 内层 packet。

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::RawFd;

use clap::{Args, Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::sleep;
use uuid::Uuid;

use crate::client::{
    DirectClient, TerminalAttachOptions, TerminalStream, TerminalStreamEvent,
    signed_device_relay_admission,
};
use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::{TermctlState, normalize_ws_url, resolve_state_path};
use termd_proto::{
    AttachRole, PairingQrPayload, PublicKey, RelayAdmissionPayload, ServerId, SessionId,
    SessionState, SessionSummaryPayload, TerminalSize,
};

pub const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";
const STDIN_BUFFER_SIZE: usize = 8 * 1024;
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(150);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(2);
const RECONNECT_MAX_ATTEMPTS: u32 = 30;

#[derive(Debug, Parser)]
#[command(name = "termctl", version, about = "termd direct WebSocket CLI")]
pub struct Cli {
    /// 覆盖本地状态文件路径；默认优先 TERMD_CTL_STATE，然后是 $HOME/.termd/termctl-state.json。
    #[arg(long, global = true)]
    pub state: Option<PathBuf>,

    /// 输出机器可读 JSON；attach 的终端字节流仍保留在 stdout。
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 使用一次性 token 配对当前设备。
    Pair(PairArgs),
    /// 创建持久 session；`--` 后的参数作为 daemon 执行命令。
    New(NewArgs),
    /// attach 到已有 session，并桥接 stdin/stdout。
    Attach(SessionUrlArgs),
    /// 关闭持久 session，并让 daemon 终止对应 PTY/supervisor。
    Close(SessionUrlArgs),
    /// 确认当前设备已作为 shared-control operator attach 到指定 session。
    Control(SessionUrlArgs),
    /// 调整指定 session 的终端尺寸。
    Resize(ResizeArgs),
    /// 列出 daemon 当前已知 session。
    List(UrlArgs),
}

#[derive(Debug, Args)]
pub struct PairArgs {
    #[arg(value_name = "invite_or_token", conflicts_with_all = ["token", "payload"])]
    pub invite_or_token: Option<String>,
    #[arg(long, conflicts_with_all = ["invite_or_token", "payload"])]
    pub token: Option<String>,
    #[arg(long, help = "Pairing invite code or legacy JSON payload", conflicts_with_all = ["invite_or_token", "token"])]
    pub payload: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    #[arg(long)]
    pub url: Option<String>,
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SessionUrlArgs {
    pub session_id: String,
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ResizeArgs {
    pub session_id: String,
    #[arg(long)]
    pub rows: u16,
    #[arg(long)]
    pub cols: u16,
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct UrlArgs {
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct OutputMode {
    json: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    let state_path = resolve_state_path(cli.state);
    let output = OutputMode { json: cli.json };
    crate::error::set_json_output(cli.json);

    match cli.command {
        Command::Pair(args) => pair(args, state_path, output).await,
        Command::New(args) => new_session(args, state_path, output).await,
        Command::Attach(args) => attach(args, state_path, output).await,
        Command::Close(args) => close(args, state_path, output).await,
        Command::Control(args) => control(args, state_path, output).await,
        Command::Resize(args) => resize(args, state_path, output).await,
        Command::List(args) => list(args, state_path, output).await,
    }
}

async fn pair(args: PairArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let mut state = TermctlState::load(&state_path)?;
    let input = PairingInput::from_args(args, &state)?;
    let device = ensure_pairing_device_persisted(&mut state, &state_path)?;
    let mut client = DirectClient::connect(
        &input.url,
        input.route_server_id,
        device.device_id,
        input.daemon_public_key.clone(),
        Some(RelayAdmissionPayload::PairTicket {
            token: termd_proto::PairingToken(input.token.clone()),
        }),
    )
    .await?;
    let accepted = client
        .pair(device.device_public_key.clone(), input.token)
        .await?;
    if accepted.server_id != input.route_server_id
        || accepted.daemon_public_key != input.daemon_public_key
    {
        drop(client);
        return Err(TermctlError::PairingPayloadServerMismatch);
    }

    state.record_pairing(accepted.clone(), input.url);
    state
        .save(&state_path)
        .map_err(|_| TermctlError::PairingStateFinalizeFailed)?;
    print_pair_accepted(output, &accepted);
    Ok(())
}

fn ensure_pairing_device_persisted(
    state: &mut TermctlState,
    state_path: &Path,
) -> Result<crate::state::DeviceState> {
    let had_device = state.device.is_some();
    let device = state.ensure_device();
    if !had_device {
        // 新设备身份必须先落盘，再把公钥发给 daemon 接受配对；否则成功配对后本机可能丢失私钥。
        state.save(state_path)?;
    }
    Ok(device)
}

#[derive(Debug)]
struct PairingInput {
    token: String,
    url: String,
    route_server_id: ServerId,
    daemon_public_key: PublicKey,
}

impl PairingInput {
    fn from_args(args: PairArgs, state: &TermctlState) -> Result<Self> {
        let PairArgs {
            invite_or_token,
            token,
            payload,
            url,
        } = args;

        if let Some(raw_invite) = payload.or_else(|| {
            invite_or_token
                .as_deref()
                .filter(|value| looks_like_pairing_invite(value))
                .map(str::to_owned)
        }) {
            return Self::from_invite(raw_invite, url.as_deref(), state);
        }

        let token = token
            .or(invite_or_token)
            .filter(|token| !token.trim().is_empty())
            .ok_or(TermctlError::InvalidPairingInvite)?;
        let route_server_id = state
            .selected_route_server_id()
            .ok_or(TermctlError::TokenRequiresKnownDaemon)?;
        let url = state.selected_url_for_server(route_server_id, url.as_deref())?;
        let daemon_public_key = state
            .paired_server(route_server_id)
            .map(|server| server.daemon_public_key)
            .ok_or(TermctlError::TokenRequiresKnownDaemon)?;
        Ok(Self {
            token,
            url,
            route_server_id,
            daemon_public_key,
        })
    }

    fn from_invite(
        raw_invite: String,
        requested_url: Option<&str>,
        state: &TermctlState,
    ) -> Result<Self> {
        let payload = parse_pairing_payload(&raw_invite)?;
        let url = pairing_payload_url(payload.ws_url.as_deref(), requested_url)?;
        let token = payload.token.0.clone();
        Ok(Self {
            token,
            url,
            route_server_id: payload.server_id,
            daemon_public_key: pairing_payload_daemon_public_key(&payload, state)?,
        })
    }
}

fn parse_pairing_payload(raw_payload: &str) -> Result<PairingQrPayload> {
    let payload = PairingQrPayload::parse_invite_code(raw_payload)
        .ok_or(TermctlError::InvalidPairingInvite)?;

    if !payload.is_supported_version()
        || payload.token.0.is_empty()
        || payload
            .ws_url
            .as_deref()
            .is_some_and(|ws_url| normalize_ws_url(ws_url).is_none())
    {
        return Err(TermctlError::InvalidPairingInvite);
    }
    if payload.expires_at_ms.0 <= crypto::now_ms().0 {
        return Err(TermctlError::ExpiredPairingInvite);
    }

    Ok(payload)
}

fn looks_like_pairing_invite(value: &str) -> bool {
    let trimmed = value.trim_start();
    trimmed.starts_with("termd-pair:") || trimmed.starts_with('{')
}

fn pairing_payload_url(
    payload_ws_url: Option<&str>,
    requested_url: Option<&str>,
) -> Result<String> {
    // 新 invite 主要由 server_id 定位 daemon；URL 只描述传输入口。旧 invite 的 ws_url
    // 仍可作为兼容回退，但会先收敛到统一 `/ws`，避免继续连接旧 `/client` 路径。
    if let Some(url) = requested_url {
        return normalize_ws_url(url).ok_or(TermctlError::InvalidWsUrl);
    }
    let url = payload_ws_url.ok_or(TermctlError::MissingPairingUrl)?;
    normalize_ws_url(url).ok_or(TermctlError::InvalidPairingInvite)
}

fn pairing_payload_daemon_public_key(
    payload: &PairingQrPayload,
    state: &TermctlState,
) -> Result<PublicKey> {
    if let Some(daemon_public_key) = &payload.daemon_public_key {
        return Ok(daemon_public_key.clone());
    }

    // 旧 invite 没有 daemon 公钥时，只允许借用本地已配对状态中的 trust anchor。
    // 未知 daemon 的首次配对必须携带 daemon_public_key，否则无法验证 E2EE 握手签名。
    state
        .paired_server(payload.server_id)
        .map(|server| server.daemon_public_key)
        .ok_or(TermctlError::InvalidPairingInvite)
}

async fn new_session(args: NewArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let created = client
        .create_session(args.command, TerminalSize::default())
        .await?;

    if output.json {
        println!(
            "{}",
            serde_json::json!({
                "session_id": created.session_id.0.to_string(),
                "role": role_name(created.role),
                "state": state_name(created.state),
                "size": terminal_size_json(created.size),
            })
        );
    } else {
        println!(
            "session={} role={} state={} size={}x{}",
            created.session_id.0,
            role_name(created.role),
            state_name(created.state),
            created.size.rows,
            created.size.cols
        );
    }
    Ok(())
}

async fn attach(args: SessionUrlArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let requested_url = args.url.clone();
    let attached_terminal =
        connect_attach_for_terminal(&state_path, requested_url.as_deref(), session_id, None)
            .await?;
    let attached = attached_terminal.attached;

    if output.json {
        eprintln!(
            "{}",
            serde_json::json!({
                "attached": {
                    "session_id": attached.session_id.0.to_string(),
                    "role": role_name(attached.role),
                    "state": state_name(attached.state),
                    "size": terminal_size_json(attached.size),
                }
            })
        );
    } else {
        eprintln!(
            "attached session={} role={} state={} size={}x{}",
            attached.session_id.0,
            role_name(attached.role),
            state_name(attached.state),
            attached.size.rows,
            attached.size.cols
        );
    }

    let _raw_mode = TerminalRawModeGuard::for_attach_stdio()?;
    attach_loop(
        attached_terminal.client,
        attached_terminal.stream,
        session_id,
        AttachReconnectContext {
            state_path,
            requested_url,
        },
    )
    .await
}

async fn control(args: SessionUrlArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;

    let stream = attach_for_session_operation(&mut client, session_id).await?;
    let granted = client.request_control(session_id).await?;
    let _ = client.cancel_terminal_stream(&stream).await;

    if output.json {
        println!(
            "{}",
            serde_json::json!({
                "control_granted": {
                    "session_id": granted.session_id.0.to_string(),
                    "device_id": granted.device_id.0.to_string(),
                }
            })
        );
    } else {
        println!(
            "control_granted session={} device={}",
            granted.session_id.0, granted.device_id.0
        );
    }
    Ok(())
}

async fn close(args: SessionUrlArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    // daemon 把 close 也视为 session 级操作，短命令需要先建立当前连接的临时 attach。
    let stream = attach_for_session_operation(&mut client, session_id).await?;
    let closed_result = client.close_session(session_id).await;
    let _ = client.cancel_terminal_stream(&stream).await;
    let closed = closed_result?;
    if output.json {
        println!(
            "{}",
            serde_json::json!({
                "closed": {
                    "session_id": closed.session_id.0.to_string(),
                }
            })
        );
    } else {
        println!("closed session={}", closed.session_id.0);
    }
    Ok(())
}

async fn resize(args: ResizeArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    if args.rows == 0 || args.cols == 0 {
        return Err(TermctlError::InvalidSize);
    }

    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let size = TerminalSize::new(args.rows, args.cols);

    let stream = attach_for_session_operation(&mut client, session_id).await?;
    client.resize_session(session_id, size).await?;
    let _ = client.cancel_terminal_stream(&stream).await;
    if output.json {
        println!(
            "{}",
            serde_json::json!({
                "resized": {
                    "session_id": session_id.0.to_string(),
                    "size": terminal_size_json(size),
                }
            })
        );
    } else {
        println!(
            "resized session={} size={}x{}",
            session_id.0, size.rows, size.cols
        );
    }
    Ok(())
}

async fn list(args: UrlArgs, state_path: PathBuf, output: OutputMode) -> Result<()> {
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let result = client.list_sessions().await?;

    if output.json {
        let sessions = result
            .sessions
            .into_iter()
            .map(session_json)
            .collect::<Vec<_>>();
        println!("{}", serde_json::json!({ "sessions": sessions }));
        return Ok(());
    }

    if result.sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    for session in result.sessions {
        println!(
            "session={} name={} state={} size={}x{}",
            session.session_id.0,
            session
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("-"),
            state_name(session.state),
            session.size.rows,
            session.size.cols
        );
    }
    Ok(())
}

async fn connect_authenticated(
    state_path: &Path,
    requested_url: Option<&str>,
) -> Result<DirectClient> {
    let state = TermctlState::load(state_path)?;
    let device = state.require_device()?;
    let target = state.selected_paired_target(requested_url)?;
    let signing_key = crypto::decode_signing_key(&device.device_signing_key_secret)?;
    let mut client = DirectClient::connect(
        &target.url,
        target.server.server_id,
        device.device_id,
        target.server.daemon_public_key.clone(),
        Some(signed_device_relay_admission(
            target.server.server_id,
            device.device_id,
            &signing_key,
        )),
    )
    .await?;

    client.authenticate(&signing_key, &target.server).await?;
    Ok(client)
}

struct AttachedTerminal {
    client: DirectClient,
    stream: TerminalStream,
    attached: termd_proto::SessionAttachedPayload,
}

struct AttachReconnectContext {
    state_path: PathBuf,
    requested_url: Option<String>,
}

async fn connect_attach_for_terminal(
    state_path: &Path,
    requested_url: Option<&str>,
    session_id: SessionId,
    last_terminal_seq: Option<u64>,
) -> Result<AttachedTerminal> {
    let mut client = connect_authenticated(state_path, requested_url).await?;
    let (attached, stream) = client
        .attach_terminal_stream_with_options(
            session_id,
            resume_terminal_attach_options(last_terminal_seq),
        )
        .await?;

    Ok(AttachedTerminal {
        client,
        stream,
        attached,
    })
}

async fn attach_for_session_operation(
    client: &mut DirectClient,
    session_id: SessionId,
) -> Result<crate::client::TerminalStream> {
    // daemon 现在把 session 作用域能力绑定到“当前 WebSocket 连接”。
    // resize owner 只会授予 watch/terminal stream attach；短命令用临时 stream，用完后 cancel。
    let (_attached, stream) = client
        .attach_terminal_stream_with_options(
            session_id,
            TerminalAttachOptions {
                watch_updates: false,
                last_terminal_seq: None,
            },
        )
        .await?;
    Ok(stream)
}

async fn attach_loop(
    mut client: DirectClient,
    mut stream: TerminalStream,
    session_id: SessionId,
    reconnect: AttachReconnectContext,
) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buffer = vec![0_u8; STDIN_BUFFER_SIZE];
    let mut stdin_open = true;
    let mut resize_state = TerminalResizeState::default();
    let mut resize_events = TerminalResizeEvents::new();

    sync_current_terminal_size_with_reconnect(
        &mut client,
        &mut stream,
        session_id,
        &reconnect,
        &mut resize_state,
    )
    .await?;

    loop {
        tokio::select! {
            read = stdin.read(&mut buffer), if stdin_open => {
                let read = match read {
                    Ok(read) => read,
                    Err(_) => {
                        // 本地输入失败时显式取消 stream，避免 daemon 继续等待这一端发送数据。
                        let _ = client.cancel_terminal_stream(&stream).await;
                        return Err(TermctlError::LocalIo);
                    }
                };
                if read == 0 {
                    stdin_open = false;
                    continue;
                }
                let bytes = buffer[..read].to_vec();
                send_terminal_data_with_reconnect(
                    &mut client,
                    &mut stream,
                    session_id,
                    &reconnect,
                    &mut resize_state,
                    &bytes,
                )
                .await?;
            }
            message = client.receive_terminal_event(&mut stream) => {
                match message {
                    Ok(TerminalStreamEvent::Output(bytes)) => {
                        stdout
                            .write_all(&bytes)
                            .await
                            .map_err(|_| TermctlError::LocalIo)?;
                        stdout.flush().await.map_err(|_| TermctlError::LocalIo)?;
                    }
                    Ok(TerminalStreamEvent::End) => return Ok(()),
                    Err(error) if error.is_connection_error() => {
                        reconnect_terminal(&mut client, &mut stream, session_id, &reconnect).await?;
                        resize_state.reset_after_reconnect();
                        sync_current_terminal_size_with_reconnect(
                            &mut client,
                            &mut stream,
                            session_id,
                            &reconnect,
                            &mut resize_state,
                        )
                        .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            size = resize_events.changed_size() => {
                if let Some(size) = size {
                    resize_terminal_with_reconnect(
                        &mut client,
                        &mut stream,
                        session_id,
                        &reconnect,
                        &mut resize_state,
                        size,
                    )
                    .await?;
                }
            }
        }
    }
}

fn resume_terminal_attach_options(last_terminal_seq: Option<u64>) -> TerminalAttachOptions {
    TerminalAttachOptions {
        watch_updates: true,
        last_terminal_seq,
    }
}

async fn send_terminal_data_with_reconnect(
    client: &mut DirectClient,
    stream: &mut TerminalStream,
    session_id: SessionId,
    reconnect: &AttachReconnectContext,
    resize_state: &mut TerminalResizeState,
    bytes: &[u8],
) -> Result<()> {
    loop {
        match client.send_terminal_data(stream, session_id, bytes).await {
            Ok(()) => return Ok(()),
            Err(error) if error.is_connection_error() => {
                reconnect_terminal(client, stream, session_id, reconnect).await?;
                resize_state.reset_after_reconnect();
                sync_current_terminal_size_with_reconnect(
                    client,
                    stream,
                    session_id,
                    reconnect,
                    resize_state,
                )
                .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn sync_current_terminal_size_with_reconnect(
    client: &mut DirectClient,
    stream: &mut TerminalStream,
    session_id: SessionId,
    reconnect: &AttachReconnectContext,
    resize_state: &mut TerminalResizeState,
) -> Result<()> {
    let Some(size) = local_terminal_size() else {
        return Ok(());
    };

    resize_terminal_with_reconnect(client, stream, session_id, reconnect, resize_state, size).await
}

async fn resize_terminal_with_reconnect(
    client: &mut DirectClient,
    stream: &mut TerminalStream,
    session_id: SessionId,
    reconnect: &AttachReconnectContext,
    resize_state: &mut TerminalResizeState,
    size: TerminalSize,
) -> Result<()> {
    loop {
        match resize_state
            .resize_if_changed(client, session_id, size)
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) if error.is_connection_error() => {
                reconnect_terminal(client, stream, session_id, reconnect).await?;
                resize_state.reset_after_reconnect();
            }
            Err(error) => return Err(error),
        }
    }
}

async fn reconnect_terminal(
    client: &mut DirectClient,
    stream: &mut TerminalStream,
    session_id: SessionId,
    reconnect: &AttachReconnectContext,
) -> Result<()> {
    let last_terminal_seq = stream.last_terminal_seq();
    let pending_events = stream.drain_pending_events();
    let mut attempt = 0_u32;

    loop {
        if attempt >= RECONNECT_MAX_ATTEMPTS {
            return Err(TermctlError::ReconnectExhausted);
        }
        attempt = attempt.saturating_add(1);
        sleep(reconnect_delay(attempt)).await;

        match connect_attach_for_terminal(
            &reconnect.state_path,
            reconnect.requested_url.as_deref(),
            session_id,
            last_terminal_seq,
        )
        .await
        {
            Ok(mut attached) => {
                attached.stream.prepend_pending_events(pending_events);
                *client = attached.client;
                *stream = attached.stream;
                return Ok(());
            }
            Err(error) if error.is_connection_error() => continue,
            Err(error) => return Err(error),
        }
    }
}

fn reconnect_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.saturating_sub(1).min(4);
    RECONNECT_INITIAL_DELAY
        .saturating_mul(multiplier)
        .min(RECONNECT_MAX_DELAY)
}

#[derive(Default)]
struct TerminalResizeState {
    last_sent: Option<TerminalSize>,
}

impl TerminalResizeState {
    fn reset_after_reconnect(&mut self) {
        // 中文注释：重连后的远端 resize owner 状态可能已经变化；即使本地尺寸没变，
        // 也要强制重新发送一次当前尺寸，避免断线期间被其他 operator 改过后不恢复。
        self.last_sent = None;
    }

    async fn resize_if_changed(
        &mut self,
        client: &mut DirectClient,
        session_id: SessionId,
        size: TerminalSize,
    ) -> Result<()> {
        if self.last_sent == Some(size) {
            return Ok(());
        }

        client.resize_session(session_id, size).await?;
        self.last_sent = Some(size);
        Ok(())
    }
}

struct TerminalResizeEvents {
    #[cfg(unix)]
    signal: Option<Signal>,
}

impl TerminalResizeEvents {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            signal: signal(SignalKind::window_change()).ok(),
        }
    }

    #[cfg(unix)]
    async fn changed_size(&mut self) -> Option<TerminalSize> {
        let Some(signal) = self.signal.as_mut() else {
            std::future::pending::<()>().await;
            return None;
        };

        loop {
            if signal.recv().await.is_none() {
                std::future::pending::<()>().await;
                return None;
            }
            if let Some(size) = local_terminal_size() {
                return Some(size);
            }
        }
    }

    #[cfg(not(unix))]
    async fn changed_size(&mut self) -> Option<TerminalSize> {
        std::future::pending::<()>().await;
        None
    }
}

fn should_enable_raw_mode_for_attach(stdin_tty: bool, stdout_tty: bool) -> bool {
    stdin_tty && stdout_tty
}

#[cfg(unix)]
struct TerminalRawModeGuard {
    fd: RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl TerminalRawModeGuard {
    fn for_attach_stdio() -> Result<Option<Self>> {
        if !should_enable_raw_mode_for_attach(
            fd_is_tty(libc::STDIN_FILENO),
            fd_is_tty(libc::STDOUT_FILENO),
        ) {
            return Ok(None);
        }

        Self::enable(libc::STDIN_FILENO).map(Some)
    }

    fn enable(fd: RawFd) -> Result<Self> {
        // 中文注释：只保存 stdin 的 termios；stdout/stderr 不参与行规程，不能乱改。
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(TermctlError::LocalIo);
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(TermctlError::LocalIo);
        }

        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for TerminalRawModeGuard {
    fn drop(&mut self) {
        // 中文注释：Drop 路径不能再返回错误；尽最大努力恢复用户终端。
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

#[cfg(not(unix))]
struct TerminalRawModeGuard;

#[cfg(not(unix))]
impl TerminalRawModeGuard {
    fn for_attach_stdio() -> Result<Option<Self>> {
        Ok(None)
    }
}

fn local_terminal_size() -> Option<TerminalSize> {
    #[cfg(unix)]
    {
        [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO]
            .into_iter()
            .find_map(terminal_size_from_fd)
    }

    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn terminal_size_from_fd(fd: RawFd) -> Option<TerminalSize> {
    if !fd_is_tty(fd) {
        return None;
    }

    let mut winsize = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, winsize.as_mut_ptr()) } != 0 {
        return None;
    }
    let winsize = unsafe { winsize.assume_init() };
    terminal_size_from_winsize(
        winsize.ws_row,
        winsize.ws_col,
        winsize.ws_xpixel,
        winsize.ws_ypixel,
    )
}

#[cfg(unix)]
fn fd_is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn terminal_size_from_winsize(
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
) -> Option<TerminalSize> {
    if rows == 0 || cols == 0 {
        return None;
    }

    Some(TerminalSize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    })
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map(SessionId)
        .map_err(|_| TermctlError::InvalidSessionId)
}

fn role_name(role: AttachRole) -> &'static str {
    match role {
        AttachRole::Operator => "operator",
    }
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::Closed => "closed",
    }
}

fn print_pair_accepted(output: OutputMode, accepted: &termd_proto::PairAcceptPayload) {
    if output.json {
        println!(
            "{}",
            serde_json::json!({
                "paired": {
                    "server_id": accepted.server_id.0.to_string(),
                    "device_id": accepted.device_id.0.to_string(),
                }
            })
        );
    } else {
        println!(
            "paired server={} device={}",
            accepted.server_id.0, accepted.device_id.0
        );
    }
}

fn terminal_size_json(size: TerminalSize) -> serde_json::Value {
    serde_json::json!({
        "rows": size.rows,
        "cols": size.cols,
        "pixel_width": size.pixel_width,
        "pixel_height": size.pixel_height,
    })
}

fn session_json(session: SessionSummaryPayload) -> serde_json::Value {
    serde_json::json!({
        "session_id": session.session_id.0.to_string(),
        "name": session.name,
        "state": state_name(session.state),
        "size": terminal_size_json(session.size),
        "files_path": session.files_path,
        "created_at_ms": session.created_at_ms.map(|timestamp| timestamp.0),
    })
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use clap::Parser;
    use termd_proto::UnixTimestampMillis;

    use super::*;

    const FUTURE_PAIRING_EXPIRES_AT_MS: u64 = 4_102_444_800_000;

    fn daemon_public_key() -> PublicKey {
        PublicKey("ed25519-v1:daemon-public".to_owned())
    }

    fn pairing_payload_json(extra_fields: &str) -> String {
        format!(
            "{{\"type\":\"termd_pairing_qr\",\"version\":2,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"{}\",\"expires_at_ms\":{FUTURE_PAIRING_EXPIRES_AT_MS}{extra_fields}}}",
            daemon_public_key().0,
        )
    }

    fn invite_from_json(payload: &str) -> String {
        format!(
            "termd-pair:v2:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        )
    }

    #[test]
    fn parses_pair_command_with_url() {
        let cli = Cli::try_parse_from([
            "termctl",
            "pair",
            "--token",
            "termd-pair-redacted",
            "--url",
            "ws://127.0.0.1:8765/ws",
        ])
        .unwrap();

        match cli.command {
            Command::Pair(args) => {
                assert_eq!(args.invite_or_token, None);
                assert_eq!(args.token.as_deref(), Some("termd-pair-redacted"));
                assert_eq!(args.payload, None);
                assert_eq!(args.url.as_deref(), Some("ws://127.0.0.1:8765/ws"));
            }
            _ => panic!("expected pair command"),
        }
    }

    #[test]
    fn parses_pair_command_with_payload() {
        let payload = "{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":1710000060000}";
        let payload = invite_from_json(payload);
        let cli = Cli::try_parse_from(["termctl", "pair", "--payload", &payload]).unwrap();

        match cli.command {
            Command::Pair(args) => {
                assert_eq!(args.invite_or_token, None);
                assert_eq!(args.token, None);
                assert!(args.payload.is_some());
            }
            _ => panic!("expected pair command"),
        }
    }

    #[test]
    fn parses_pair_command_with_positional_invite() {
        let payload = invite_from_json(&pairing_payload_json(""));
        let cli = Cli::try_parse_from(["termctl", "pair", &payload]).unwrap();

        match cli.command {
            Command::Pair(args) => {
                assert_eq!(args.invite_or_token.as_deref(), Some(payload.as_str()));
                assert_eq!(args.token, None);
                assert_eq!(args.payload, None);
            }
            _ => panic!("expected pair command"),
        }
    }

    #[test]
    fn parses_global_json_flag() {
        let cli = Cli::try_parse_from(["termctl", "--json", "list"]).unwrap();

        assert!(cli.json);
        assert!(matches!(cli.command, Command::List(_)));
    }

    #[test]
    fn ensure_pairing_device_persists_identity_before_pair_request() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let mut state = TermctlState::default();

        let device = ensure_pairing_device_persisted(&mut state, &state_path).unwrap();
        let saved = TermctlState::load(&state_path).unwrap();

        assert_eq!(state.device, Some(device.clone()));
        assert_eq!(saved.device, Some(device));
        assert!(saved.paired_servers.is_empty());
    }

    #[test]
    fn pairing_payload_with_explicit_url_uses_override_when_missing_ws_url() {
        let server_id =
            ServerId(uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let payload = invite_from_json(&pairing_payload_json(""));
        let args = PairArgs {
            invite_or_token: None,
            token: None,
            payload: Some(payload),
            url: Some("ws://127.0.0.1:8765/ws".to_owned()),
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();
        assert_eq!(input.url, DEFAULT_URL);
        assert_eq!(input.route_server_id, server_id);
        assert_eq!(input.daemon_public_key, daemon_public_key());
    }

    #[test]
    fn pairing_payload_legacy_ws_url_is_normalized_to_unified_ws_endpoint() {
        let payload = invite_from_json(&pairing_payload_json(
            ",\"ws_url\":\"wss://relay.example/termd/ws/00000000-0000-0000-0000-000000000001/client?relay_token=redacted\"",
        ));
        let args = PairArgs {
            invite_or_token: None,
            token: None,
            payload: Some(payload),
            url: None,
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(
            input.url,
            "wss://relay.example/termd/ws?relay_token=redacted"
        );
    }

    #[test]
    fn explicit_pair_url_overrides_legacy_payload_ws_url() {
        let payload = invite_from_json(&pairing_payload_json(
            ",\"ws_url\":\"wss://legacy.example/ws/00000000-0000-0000-0000-000000000001/client\"",
        ));
        let args = PairArgs {
            invite_or_token: None,
            token: None,
            payload: Some(payload),
            url: Some("wss://relay.example/ws".to_owned()),
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(input.url, "wss://relay.example/ws");
    }

    #[test]
    fn pairing_payload_without_daemon_public_key_requires_known_server_state() {
        let server_id =
            ServerId(uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let payload = format!(
            "{{\"type\":\"termd_pairing_qr\",\"version\":2,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":{FUTURE_PAIRING_EXPIRES_AT_MS}}}"
        );
        let args = PairArgs {
            invite_or_token: None,
            token: None,
            payload: Some(invite_from_json(&payload)),
            url: None,
        };
        let empty_state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &empty_state).unwrap_err(),
            TermctlError::MissingPairingUrl
        ));

        let args = PairArgs {
            invite_or_token: None,
            token: None,
            payload: Some(invite_from_json(&payload)),
            url: Some(DEFAULT_URL.to_owned()),
        };
        let state = TermctlState {
            paired_servers: vec![crate::state::PairedServerState {
                server_id,
                daemon_public_key: daemon_public_key(),
                url: DEFAULT_URL.to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            ..TermctlState::default()
        };

        let input = PairingInput::from_args(args, &state).unwrap();
        assert_eq!(input.daemon_public_key, daemon_public_key());
    }

    #[test]
    fn token_only_pairing_requires_known_daemon() {
        let args = PairArgs {
            invite_or_token: None,
            token: Some("pair-token".to_owned()),
            payload: None,
            url: None,
        };
        let state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &state).unwrap_err(),
            TermctlError::TokenRequiresKnownDaemon
        ));
    }

    #[test]
    fn token_only_pairing_uses_known_daemon_route_and_url() {
        let server_id = ServerId::new();
        let args = PairArgs {
            invite_or_token: None,
            token: Some("pair-token".to_owned()),
            payload: None,
            url: None,
        };
        let state = TermctlState {
            default_server_id: Some(server_id),
            default_url: Some(format!("ws://127.0.0.1:8765/ws/{}/client", server_id.0)),
            paired_servers: vec![crate::state::PairedServerState {
                server_id,
                daemon_public_key: daemon_public_key(),
                url: "ws://127.0.0.1:8765/ws".to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            ..TermctlState::default()
        };

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(input.route_server_id, server_id);
        assert_eq!(input.url, "ws://127.0.0.1:8765/ws");
        assert_eq!(input.daemon_public_key, daemon_public_key());
    }

    #[test]
    fn positional_pair_invite_uses_invite_payload() {
        let server_id =
            ServerId(uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let payload = invite_from_json(&pairing_payload_json(
            ",\"ws_url\":\"wss://relay.example/ws\"",
        ));
        let args = PairArgs {
            invite_or_token: Some(payload),
            token: None,
            payload: None,
            url: None,
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(input.token, "pair-token");
        assert_eq!(input.url, "wss://relay.example/ws");
        assert_eq!(input.route_server_id, server_id);
    }

    #[test]
    fn invalid_pair_invite_has_specific_error() {
        let args = PairArgs {
            invite_or_token: Some("termd-pair:v1:not-base64".to_owned()),
            token: None,
            payload: None,
            url: None,
        };
        let state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &state).unwrap_err(),
            TermctlError::InvalidPairingInvite
        ));
    }

    #[test]
    fn expired_pair_invite_has_specific_error() {
        let expired_payload = "{\"type\":\"termd_pairing_qr\",\"version\":2,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"ed25519-v1:daemon-public\",\"expires_at_ms\":1}";
        let args = PairArgs {
            invite_or_token: Some(invite_from_json(expired_payload)),
            token: None,
            payload: None,
            url: Some(DEFAULT_URL.to_owned()),
        };
        let state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &state).unwrap_err(),
            TermctlError::ExpiredPairingInvite
        ));
    }

    #[test]
    fn pair_invite_without_url_or_override_has_specific_error() {
        let args = PairArgs {
            invite_or_token: Some(invite_from_json(&pairing_payload_json(""))),
            token: None,
            payload: None,
            url: None,
        };
        let state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &state).unwrap_err(),
            TermctlError::MissingPairingUrl
        ));
    }

    #[test]
    fn parses_new_command_after_double_dash() {
        let cli = Cli::try_parse_from(["termctl", "new", "--", "bash", "-lc", "echo ok"]).unwrap();

        match cli.command {
            Command::New(args) => assert_eq!(args.command, ["bash", "-lc", "echo ok"]),
            _ => panic!("expected new command"),
        }
    }

    #[test]
    fn parses_attach_control_resize_and_list() {
        let session_id = Uuid::new_v4().to_string();

        assert!(matches!(
            Cli::try_parse_from(["termctl", "attach", &session_id])
                .unwrap()
                .command,
            Command::Attach(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["termctl", "control", &session_id])
                .unwrap()
                .command,
            Command::Control(_)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "termctl",
                "resize",
                &session_id,
                "--rows",
                "40",
                "--cols",
                "120"
            ])
            .unwrap()
            .command,
            Command::Resize(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["termctl", "list"]).unwrap().command,
            Command::List(_)
        ));
    }

    #[test]
    fn invalid_session_id_is_user_error() {
        assert!(matches!(
            parse_session_id("not-a-uuid").unwrap_err(),
            TermctlError::InvalidSessionId
        ));
    }

    #[test]
    fn attach_raw_mode_only_for_interactive_tty() {
        assert!(should_enable_raw_mode_for_attach(true, true));
        assert!(!should_enable_raw_mode_for_attach(false, true));
        assert!(!should_enable_raw_mode_for_attach(true, false));
        assert!(!should_enable_raw_mode_for_attach(false, false));
    }

    #[test]
    fn terminal_size_from_winsize_ignores_missing_cells_and_preserves_pixels() {
        assert_eq!(
            terminal_size_from_winsize(33, 111, 999, 777),
            Some(TerminalSize {
                rows: 33,
                cols: 111,
                pixel_width: 999,
                pixel_height: 777,
            })
        );
        assert_eq!(terminal_size_from_winsize(0, 111, 999, 777), None);
        assert_eq!(terminal_size_from_winsize(33, 0, 999, 777), None);
    }

    #[test]
    fn reconnect_resume_options_keep_last_terminal_sequence() {
        assert_eq!(
            resume_terminal_attach_options(Some(41)),
            TerminalAttachOptions {
                watch_updates: true,
                last_terminal_seq: Some(41),
            }
        );
        assert_eq!(
            resume_terminal_attach_options(None),
            TerminalAttachOptions {
                watch_updates: true,
                last_terminal_seq: None,
            }
        );
    }

    #[test]
    fn terminal_resize_state_forces_sync_after_reconnect() {
        let size = TerminalSize::new(24, 80);
        let mut resize_state = TerminalResizeState {
            last_sent: Some(size),
        };

        resize_state.reset_after_reconnect();

        assert_eq!(resize_state.last_sent, None);
    }

    #[test]
    fn reconnect_attempt_budget_is_finite() {
        let reconnect_max_attempts = RECONNECT_MAX_ATTEMPTS;
        assert!(reconnect_max_attempts > 0);
        assert!(reconnect_max_attempts <= 60);
    }
}
