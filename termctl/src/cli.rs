//! termctl 命令分发。
//!
//! CLI 默认面向人类输出：session/control/list 结果写 stdout，attach 的状态提示写 stderr，
//! 这样不会污染终端业务字节流。所有敏感输入只进入 E2EE 内层 envelope。

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::interval;
use uuid::Uuid;

use crate::client::DirectClient;
use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::{TermctlState, normalize_ws_url, resolve_state_path};
use termd_proto::{
    AttachRole, ErrorPayload, MessageType, PairingQrPayload, ServerId, SessionDataPayload,
    SessionId, SessionState, TerminalSize,
};

pub const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";
const ATTACH_PING_INTERVAL: Duration = Duration::from_millis(200);
const STDIN_BUFFER_SIZE: usize = 8 * 1024;

#[derive(Debug, Parser)]
#[command(name = "termctl", version, about = "termd direct WebSocket CLI")]
pub struct Cli {
    /// 覆盖本地状态文件路径；默认优先 TERMD_CTL_STATE，然后是 $HOME/.termd/termctl-state.json。
    #[arg(long, global = true)]
    pub state: Option<PathBuf>,

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
    /// 确认当前设备已作为 shared-control operator attach 到指定 session。
    Control(SessionUrlArgs),
    /// 调整指定 session 的终端尺寸。
    Resize(ResizeArgs),
    /// 列出 daemon 当前已知 session。
    List(UrlArgs),
}

#[derive(Debug, Args)]
pub struct PairArgs {
    #[arg(long, required_unless_present = "payload", conflicts_with = "payload")]
    pub token: Option<String>,
    #[arg(long, help = "Pairing invite code or legacy JSON payload")]
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

pub async fn run(cli: Cli) -> Result<()> {
    let state_path = resolve_state_path(cli.state);

    match cli.command {
        Command::Pair(args) => pair(args, state_path).await,
        Command::New(args) => new_session(args, state_path).await,
        Command::Attach(args) => attach(args, state_path).await,
        Command::Control(args) => control(args, state_path).await,
        Command::Resize(args) => resize(args, state_path).await,
        Command::List(args) => list(args, state_path).await,
    }
}

async fn pair(args: PairArgs, state_path: PathBuf) -> Result<()> {
    let mut state = TermctlState::load(&state_path)?;
    let input = PairingInput::from_args(args, &state)?;
    let device = state.ensure_device();
    let mut client =
        DirectClient::connect(&input.url, input.route_server_id, device.device_id).await?;
    let accepted = client
        .pair(device.device_public_key.clone(), input.token)
        .await?;
    if accepted.server_id != input.route_server_id {
        drop(client);
        return Err(TermctlError::PairingPayloadServerMismatch);
    }

    state.record_pairing(accepted.clone(), input.url);
    state.save(&state_path)?;
    println!(
        "paired server={} device={}",
        accepted.server_id.0, accepted.device_id.0
    );
    Ok(())
}

#[derive(Debug)]
struct PairingInput {
    token: String,
    url: String,
    route_server_id: ServerId,
}

impl PairingInput {
    fn from_args(args: PairArgs, state: &TermctlState) -> Result<Self> {
        if let Some(raw_payload) = args.payload {
            let payload = parse_pairing_payload(&raw_payload)?;
            let url = pairing_payload_url(payload.ws_url.as_deref(), args.url.as_deref())?;
            return Ok(Self {
                token: payload.token.0,
                url,
                route_server_id: payload.server_id,
            });
        }

        let token = args.token.ok_or(TermctlError::InvalidPairingPayload)?;
        let route_server_id = state
            .selected_route_server_id()
            .ok_or(TermctlError::MissingRouteServerId)?;
        let url = state.selected_url_for_server(route_server_id, args.url.as_deref())?;
        Ok(Self {
            token,
            url,
            route_server_id,
        })
    }
}

fn parse_pairing_payload(raw_payload: &str) -> Result<PairingQrPayload> {
    let payload = PairingQrPayload::parse_invite_code(raw_payload)
        .ok_or(TermctlError::InvalidPairingPayload)?;

    if !payload.is_supported_version()
        || payload.token.0.is_empty()
        || payload
            .ws_url
            .as_deref()
            .is_some_and(|ws_url| normalize_ws_url(ws_url).is_none())
    {
        return Err(TermctlError::InvalidPairingPayload);
    }

    Ok(payload)
}

fn pairing_payload_url(
    payload_ws_url: Option<&str>,
    requested_url: Option<&str>,
) -> Result<String> {
    // 新 invite 主要由 server_id 定位 daemon；URL 只描述传输入口。旧 invite 的 ws_url
    // 仍可作为兼容回退，但会先收敛到统一 `/ws`，避免继续连接旧 `/client` 路径。
    let url = requested_url.or(payload_ws_url).unwrap_or(DEFAULT_URL);
    normalize_ws_url(url).ok_or(TermctlError::InvalidPairingPayload)
}

async fn new_session(args: NewArgs, state_path: PathBuf) -> Result<()> {
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let created = client
        .create_session(args.command, TerminalSize::default())
        .await?;

    println!(
        "session={} role={} state={} size={}x{}",
        created.session_id.0,
        role_name(created.role),
        state_name(created.state),
        created.size.rows,
        created.size.cols
    );
    Ok(())
}

async fn attach(args: SessionUrlArgs, state_path: PathBuf) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let attached = client.attach_session(session_id).await?;

    eprintln!(
        "attached session={} role={} state={} size={}x{}",
        attached.session_id.0,
        role_name(attached.role),
        state_name(attached.state),
        attached.size.rows,
        attached.size.cols
    );

    attach_loop(client, session_id).await
}

async fn control(args: SessionUrlArgs, state_path: PathBuf) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;

    attach_for_session_operation(&mut client, session_id).await?;
    let granted = client.request_control(session_id).await?;

    println!(
        "control_granted session={} device={}",
        granted.session_id.0, granted.device_id.0
    );
    Ok(())
}

async fn resize(args: ResizeArgs, state_path: PathBuf) -> Result<()> {
    if args.rows == 0 || args.cols == 0 {
        return Err(TermctlError::InvalidSize);
    }

    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let size = TerminalSize::new(args.rows, args.cols);

    attach_for_session_operation(&mut client, session_id).await?;
    client.resize_session(session_id, size).await?;
    println!(
        "resized session={} size={}x{}",
        session_id.0, size.rows, size.cols
    );
    Ok(())
}

async fn list(args: UrlArgs, state_path: PathBuf) -> Result<()> {
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let result = client.list_sessions().await?;

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
    state_path: &PathBuf,
    requested_url: Option<&str>,
) -> Result<DirectClient> {
    let state = TermctlState::load(state_path)?;
    let device = state.require_device()?;
    let target = state.selected_paired_target(requested_url)?;
    let signing_key = crypto::decode_signing_key(&device.device_signing_key_secret)?;
    let mut client =
        DirectClient::connect(&target.url, target.server.server_id, device.device_id).await?;

    client.authenticate(&signing_key, &target.server).await?;
    Ok(client)
}

async fn attach_for_session_operation(
    client: &mut DirectClient,
    session_id: SessionId,
) -> Result<()> {
    // daemon 现在把 session 作用域能力绑定到“当前 WebSocket 连接”。
    // control/resize 先 attach，避免同一设备的新连接借用旧连接在 runtime 中的 operator 状态。
    client.attach_session(session_id).await?;
    Ok(())
}

async fn attach_loop(mut client: DirectClient, session_id: SessionId) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut ticker = interval(ATTACH_PING_INTERVAL);
    let mut buffer = vec![0_u8; STDIN_BUFFER_SIZE];
    let mut stdin_open = true;

    loop {
        tokio::select! {
            read = stdin.read(&mut buffer), if stdin_open => {
                let read = read.map_err(|_| TermctlError::LocalIo)?;
                if read == 0 {
                    stdin_open = false;
                    continue;
                }
                client.send_session_data(session_id, &buffer[..read]).await?;
            }
            _ = ticker.tick() => {
                client.send_ping().await?;
            }
            message = client.receive_inner() => {
                match message {
                    Ok(envelope) => handle_attach_envelope(envelope, &mut stdout).await?,
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

async fn handle_attach_envelope(
    envelope: termd::net::protocol::JsonEnvelope,
    stdout: &mut tokio::io::Stdout,
) -> Result<()> {
    match envelope.kind {
        MessageType::SessionData => {
            let payload: SessionDataPayload =
                termd::net::protocol::decode_payload(envelope.payload)
                    .map_err(|_| TermctlError::InvalidEnvelope)?;
            let bytes = crypto::decode_session_data(&payload.data_base64)?;
            stdout
                .write_all(&bytes)
                .await
                .map_err(|_| TermctlError::LocalIo)?;
            stdout.flush().await.map_err(|_| TermctlError::LocalIo)?;
            Ok(())
        }
        MessageType::Pong => Ok(()),
        MessageType::ControlGrant
        | MessageType::DaemonClientsResult
        | MessageType::SessionAttached
        | MessageType::SessionCursor
        | MessageType::SessionFilesResult => {
            // attach 模式的 stdout 只能写入 PTY 明文字节。daemon 可能在同一条已 attach
            // 连接上推送文件树、客户端列表或 shared-control 状态，这些旁路消息对 CLI
            // 交互没有可展示价值，必须忽略而不是中断终端流。
            Ok(())
        }
        MessageType::Error => {
            let payload: ErrorPayload = termd::net::protocol::decode_payload(envelope.payload)
                .map_err(|_| TermctlError::InvalidEnvelope)?;
            Err(TermctlError::Protocol {
                code: payload.code,
                message: payload.message,
            })
        }
        _ => Err(TermctlError::UnexpectedMessage),
    }
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

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use clap::Parser;

    use super::*;

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
        let payload = format!(
            "termd-pair:v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let cli = Cli::try_parse_from(["termctl", "pair", "--payload", &payload]).unwrap();

        match cli.command {
            Command::Pair(args) => {
                assert_eq!(args.token, None);
                assert!(args.payload.is_some());
            }
            _ => panic!("expected pair command"),
        }
    }

    #[test]
    fn parses_pair_command_with_payload_and_falls_back_to_default_url_when_missing_ws_url() {
        let server_id =
            ServerId(uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let payload = "{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":1710000060000}";
        let payload = format!(
            "termd-pair:v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let args = PairArgs {
            token: None,
            payload: Some(payload),
            url: None,
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();
        assert_eq!(input.url, DEFAULT_URL);
        assert_eq!(input.route_server_id, server_id);
    }

    #[test]
    fn pairing_payload_legacy_ws_url_is_normalized_to_unified_ws_endpoint() {
        let payload = "{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":1710000060000,\"ws_url\":\"wss://relay.example/termd/ws/00000000-0000-0000-0000-000000000001/client?relay_token=redacted\"}";
        let payload = format!(
            "termd-pair:v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let args = PairArgs {
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
        let payload = "{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":1710000060000,\"ws_url\":\"wss://legacy.example/ws/00000000-0000-0000-0000-000000000001/client\"}";
        let payload = format!(
            "termd-pair:v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let args = PairArgs {
            token: None,
            payload: Some(payload),
            url: Some("wss://relay.example/ws".to_owned()),
        };
        let state = TermctlState::default();

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(input.url, "wss://relay.example/ws");
    }

    #[test]
    fn token_only_pairing_requires_known_route_server_id() {
        let args = PairArgs {
            token: Some("pair-token".to_owned()),
            payload: None,
            url: None,
        };
        let state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &state).unwrap_err(),
            TermctlError::MissingRouteServerId
        ));
    }

    #[test]
    fn token_only_pairing_uses_known_daemon_route_and_url() {
        let server_id = ServerId::new();
        let args = PairArgs {
            token: Some("pair-token".to_owned()),
            payload: None,
            url: None,
        };
        let state = TermctlState {
            default_server_id: Some(server_id),
            default_url: Some(format!("ws://127.0.0.1:8765/ws/{}/client", server_id.0)),
            ..TermctlState::default()
        };

        let input = PairingInput::from_args(args, &state).unwrap();

        assert_eq!(input.route_server_id, server_id);
        assert_eq!(input.url, "ws://127.0.0.1:8765/ws");
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
}
