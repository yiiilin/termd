//! termctl 命令分发。
//!
//! CLI 默认面向人类输出：session/control/list 结果写 stdout，attach 的状态提示写 stderr，
//! 这样不会污染终端业务字节流。所有敏感输入只进入 E2EE 内层 packet。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::client::{DirectClient, TerminalStream, TerminalStreamEvent};
use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::{TermctlState, normalize_ws_url, resolve_state_path};
use termd_proto::{
    AttachRole, PairingQrPayload, PublicKey, ServerId, SessionId, SessionState, TerminalSize,
};

pub const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";
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
        Command::Close(args) => close(args, state_path).await,
        Command::Control(args) => control(args, state_path).await,
        Command::Resize(args) => resize(args, state_path).await,
        Command::List(args) => list(args, state_path).await,
    }
}

async fn pair(args: PairArgs, state_path: PathBuf) -> Result<()> {
    let mut state = TermctlState::load(&state_path)?;
    let input = PairingInput::from_args(args, &state)?;
    let device = state.ensure_device();
    let mut client = DirectClient::connect(
        &input.url,
        input.route_server_id,
        device.device_id,
        input.daemon_public_key.clone(),
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
    daemon_public_key: PublicKey,
}

impl PairingInput {
    fn from_args(args: PairArgs, state: &TermctlState) -> Result<Self> {
        if let Some(raw_payload) = args.payload {
            let payload = parse_pairing_payload(&raw_payload)?;
            let url = pairing_payload_url(payload.ws_url.as_deref(), args.url.as_deref())?;
            let token = payload.token.0.clone();
            return Ok(Self {
                token,
                url,
                route_server_id: payload.server_id,
                daemon_public_key: pairing_payload_daemon_public_key(&payload, state)?,
            });
        }

        let token = args.token.ok_or(TermctlError::InvalidPairingPayload)?;
        let route_server_id = state
            .selected_route_server_id()
            .ok_or(TermctlError::MissingRouteServerId)?;
        let url = state.selected_url_for_server(route_server_id, args.url.as_deref())?;
        let daemon_public_key = state
            .paired_server(route_server_id)
            .map(|server| server.daemon_public_key)
            .ok_or(TermctlError::MissingPairing)?;
        Ok(Self {
            token,
            url,
            route_server_id,
            daemon_public_key,
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
        .ok_or(TermctlError::InvalidPairingPayload)
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
    let (attached, stream) = client.attach_terminal_stream(session_id).await?;

    eprintln!(
        "attached session={} role={} state={} size={}x{}",
        attached.session_id.0,
        role_name(attached.role),
        state_name(attached.state),
        attached.size.rows,
        attached.size.cols
    );

    attach_loop(client, stream, session_id).await
}

async fn control(args: SessionUrlArgs, state_path: PathBuf) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;

    let stream = attach_for_session_operation(&mut client, session_id).await?;
    let granted = client.request_control(session_id).await?;
    let _ = client.cancel_terminal_stream(&stream).await;

    println!(
        "control_granted session={} device={}",
        granted.session_id.0, granted.device_id.0
    );
    Ok(())
}

async fn close(args: SessionUrlArgs, state_path: PathBuf) -> Result<()> {
    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let closed = client.close_session(session_id).await?;
    println!("closed session={}", closed.session_id.0);
    Ok(())
}

async fn resize(args: ResizeArgs, state_path: PathBuf) -> Result<()> {
    if args.rows == 0 || args.cols == 0 {
        return Err(TermctlError::InvalidSize);
    }

    let session_id = parse_session_id(&args.session_id)?;
    let mut client = connect_authenticated(&state_path, args.url.as_deref()).await?;
    let size = TerminalSize::new(args.rows, args.cols);

    let stream = attach_for_session_operation(&mut client, session_id).await?;
    client.resize_session(session_id, size).await?;
    let _ = client.cancel_terminal_stream(&stream).await;
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
    let mut client = DirectClient::connect(
        &target.url,
        target.server.server_id,
        device.device_id,
        target.server.daemon_public_key.clone(),
    )
    .await?;

    client.authenticate(&signing_key, &target.server).await?;
    Ok(client)
}

async fn attach_for_session_operation(
    client: &mut DirectClient,
    session_id: SessionId,
) -> Result<crate::client::TerminalStream> {
    // daemon 现在把 session 作用域能力绑定到“当前 WebSocket 连接”。
    // resize owner 只会授予 watch/terminal stream attach；短命令用临时 stream，用完后 cancel。
    let (_attached, stream) = client.attach_terminal_stream(session_id).await?;
    Ok(stream)
}

async fn attach_loop(
    mut client: DirectClient,
    mut stream: TerminalStream,
    session_id: SessionId,
) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buffer = vec![0_u8; STDIN_BUFFER_SIZE];
    let mut stdin_open = true;

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
                client.send_terminal_data(&mut stream, session_id, &buffer[..read]).await?;
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
                    Err(error) => return Err(error),
                }
            }
        }
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
    use termd_proto::UnixTimestampMillis;

    use super::*;

    fn daemon_public_key() -> PublicKey {
        PublicKey("ed25519-v1:daemon-public".to_owned())
    }

    fn pairing_payload_json(extra_fields: &str) -> String {
        format!(
            "{{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"{}\",\"expires_at_ms\":1710000060000{extra_fields}}}",
            daemon_public_key().0
        )
    }

    fn invite_from_json(payload: &str) -> String {
        format!(
            "termd-pair:v1:{}",
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
        let payload = invite_from_json(&pairing_payload_json(""));
        let args = PairArgs {
            token: None,
            payload: Some(payload),
            url: None,
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
        let payload = "{\"type\":\"termd_pairing_qr\",\"version\":1,\"token\":\"pair-token\",\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"expires_at_ms\":1710000060000}";
        let args = PairArgs {
            token: None,
            payload: Some(invite_from_json(payload)),
            url: None,
        };
        let empty_state = TermctlState::default();

        assert!(matches!(
            PairingInput::from_args(args, &empty_state).unwrap_err(),
            TermctlError::InvalidPairingPayload
        ));

        let args = PairArgs {
            token: None,
            payload: Some(invite_from_json(payload)),
            url: None,
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
