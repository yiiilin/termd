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
use crate::state::{TermctlState, resolve_state_path};
use termd_proto::{
    AttachRole, ErrorPayload, MessageType, SessionDataPayload, SessionId, SessionState,
    TerminalSize,
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
    /// 抢占指定 session 的 controller。
    Control(SessionUrlArgs),
    /// 调整指定 session 的终端尺寸。
    Resize(ResizeArgs),
    /// 列出 daemon 当前已知 session。
    List(UrlArgs),
}

#[derive(Debug, Args)]
pub struct PairArgs {
    #[arg(long)]
    pub token: String,
    #[arg(long, default_value = DEFAULT_URL)]
    pub url: String,
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
    let device = state.ensure_device();
    let mut client = DirectClient::connect(&args.url, device.device_id).await?;
    let accepted = client
        .pair(device.device_public_key.clone(), args.token)
        .await?;

    state.record_pairing(accepted.clone(), args.url);
    state.save(&state_path)?;
    println!(
        "paired server={} device={}",
        accepted.server_id.0, accepted.device_id.0
    );
    Ok(())
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
            "session={} state={} size={}x{}",
            session.session_id.0,
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
    let url = state.selected_url(requested_url);
    let mut client = DirectClient::connect(&url, device.device_id).await?;
    let paired_server = state
        .paired_server(client.server_id())
        .ok_or(TermctlError::NotPaired)?;
    let signing_key = crypto::decode_signing_key(&device.device_signing_key_secret)?;

    client.authenticate(&signing_key, &paired_server).await?;
    Ok(client)
}

async fn attach_for_session_operation(
    client: &mut DirectClient,
    session_id: SessionId,
) -> Result<()> {
    // daemon 现在把 session 作用域能力绑定到“当前 WebSocket 连接”。
    // control/resize 先 attach，避免同一设备的新连接借用旧连接的 controller/viewer 角色。
    client.attach_session(session_id).await?;
    Ok(())
}

async fn attach_loop(mut client: DirectClient, session_id: SessionId) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut ticker = interval(ATTACH_PING_INTERVAL);
    let mut buffer = vec![0_u8; STDIN_BUFFER_SIZE];
    let mut stdin_open = true;
    let mut input_enabled = true;

    loop {
        tokio::select! {
            read = stdin.read(&mut buffer), if stdin_open && input_enabled => {
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
                    Err(TermctlError::Protocol { code, message }) if code == "controller_required" => {
                        // viewer 写入被 daemon 拒绝后继续保持可读输出；禁用 stdin 可避免同一输入流
                        // 反复触发 controller_required 噪音。
                        input_enabled = false;
                        eprintln!(
                            "{}",
                            TermctlError::Protocol { code, message }.user_message()
                        );
                    }
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
        AttachRole::Controller => "controller",
        AttachRole::Viewer => "viewer",
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
                assert_eq!(args.token, "termd-pair-redacted");
                assert_eq!(args.url, DEFAULT_URL);
            }
            _ => panic!("expected pair command"),
        }
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
