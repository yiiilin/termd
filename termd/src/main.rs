use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use qrcode::{Color as QrColor, QrCode, render::unicode};
use serde::Deserialize;
use termd::config::{
    DaemonConfig, SecretString, normalize_relay_endpoints, normalize_relay_proxy_url,
};
use termd::net::relay::{
    RelayBaseUrl, RelayProxyUrl, RelayReconnectPolicy, run_relay_mux_with_reconnect,
};
use termd::net::server::{TlsPaths, serve, serve_tls, try_default_protocol};
use termd::pty::supervisor::{SessionSupervisorArgs, run_session_supervisor};
use termd::pty::{CommandSpec, PtySize};
use termd_proto::{PairingQrPayload, PairingToken, PublicKey, ServerId, UnixTimestampMillis};
use tokio::task::JoinHandle;

mod process_lock;

const DEFAULT_PAIRING_URL: &str = "http://127.0.0.1:8765";
const LOCAL_PAIRING_TOKEN_PATH: &str = "/local/pairing-token";
const LOCAL_PAIRING_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const DEDICATED_RELAY_PROXY_ENV_VARS: [&str; 2] = ["TERMD_RELAY_PROXY_URL", "TERMD_RELAY_PROXY"];
const COMMON_WS_PROXY_ENV_VARS: [&str; 4] = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];
const COMMON_WSS_PROXY_ENV_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];
const NO_PROXY_ENV_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];
const HELP_TEXT: &str = concat!(
    "termd ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "USAGE:\n",
    "  termd [OPTIONS]\n",
    "  termd pair [OPTIONS]\n",
    "  termd install [OPTIONS]\n",
    "  termd uninstall [OPTIONS]\n\n",
    "OPTIONS:\n",
    "  --listen <HOST:PORT>           Listen address, default 127.0.0.1:8765\n",
    "  --relay <WS_URL>               Connect to one relay\n",
    "  --relay-url <WS_URL>           Alias for --relay\n",
    "  --relay-daemon-token <TOKEN>   Daemon admission token registered on trusted relay\n",
    "  --relay-daemon-token-file <PATH> Read trusted relay daemon admission token from a file\n",
    "  --relay-proxy <PROXY_URL>      Relay outbound proxy, http://host:port or socks5://host:port\n",
    "  --tls-cert <CERT_PEM>          TLS certificate path\n",
    "  --tls-key <KEY_PEM>            TLS private key path; must be paired with --tls-cert\n",
    "  --web                          Serve embedded Web UI\n",
    "  -h, --help                     Print help\n",
    "  -V, --version                  Print version\n\n",
    "INSTALLATION:\n",
    "  Run `termd install --help` or `termd uninstall --help` for managed installation options.\n\n",
    "PAIR OPTIONS:\n",
    "  --url <HTTP_URL>               Local daemon URL, default http://127.0.0.1:8765\n",
    "  --qr                           Print a QR invite code for Web/mobile pairing\n",
    "  --qr-svg <PATH>                 Write a real SVG QR invite code to PATH\n\n",
    "ENVIRONMENT:\n",
    "  TERMD_RELAY_PROXY_URL, TERMD_RELAY_PROXY\n",
    "  HTTPS_PROXY, HTTP_PROXY, ALL_PROXY and lowercase variants for relay outbound proxy\n",
    "  NO_PROXY and no_proxy bypass common proxy variables for matching relay hosts\n\n",
    "EXAMPLES:\n",
    "  termd --listen 0.0.0.0:8765 --web\n",
    "  termd --relay wss://relay.example:443 --relay-daemon-token-file /etc/termd/termd_daemon_token\n",
    "  HTTP_PROXY=http://127.0.0.1:3128 termd --relay wss://relay.example/ws\n",
    "  ALL_PROXY=socks5://127.0.0.1:1080 termd --relay wss://relay.example/ws\n",
    "  termd pair --url http://127.0.0.1:8765\n",
    "  termd pair --qr\n",
);

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = run().await {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

async fn run() -> Result<(), Box<dyn Error>> {
    if let Some(options) = installer_options(std::env::args_os()) {
        terminstall::run(terminstall::Component::Termd, options)?;
        return Ok(());
    }

    install_rustls_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "termd=info,tower_http=warn".into()),
        )
        .init();

    match CliCommand::parse(std::env::args().skip(1))? {
        CliCommand::Help => {
            println!("{HELP_TEXT}");
        }
        CliCommand::Version => {
            println!("termd {}", env!("CARGO_PKG_VERSION"));
        }
        CliCommand::Serve {
            listen,
            relay_urls,
            relay_daemon_token,
            relay_proxy_url,
            tls,
            web,
        } => {
            serve_daemon(
                listen,
                relay_urls,
                relay_daemon_token,
                relay_proxy_url,
                tls,
                web,
            )
            .await?
        }
        CliCommand::Pair { url, qr, qr_svg } => {
            let token = request_pairing_token_response(&url)?;
            if qr || qr_svg.is_some() {
                let payload = build_pairing_qr_payload(&token)?;
                let invite_code = payload.to_invite_code();
                if qr {
                    println!("{}", render_pairing_qr(&invite_code)?);
                }
                if let Some(path) = qr_svg.as_deref() {
                    write_pairing_qr_svg(path, &invite_code)?;
                }
                println!("{invite_code}");
            } else {
                println!("{}", token.token.0);
            }
        }
        CliCommand::SessionSupervisor(args) => run_session_supervisor(args).await?,
    }

    Ok(())
}

fn installer_options<I, S>(args: I) -> Option<terminstall::Options>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    terminstall::Options::from_subcommand(
        env!("CARGO_PKG_VERSION"),
        Some(terminstall::supervisor_version()),
        args.into_iter().skip(1),
    )
}

fn install_rustls_crypto_provider() {
    // 中文注释：reqwest 与 tokio-rustls 可能让 rustls 同时编入多个 provider；
    // 进程启动时显式选择 aws-lc，避免首次 HTTPS 注册 relay admission 时失败。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn serve_daemon(
    listen: Option<ListenAddress>,
    relay_urls: Vec<String>,
    relay_daemon_token: Option<String>,
    relay_proxy_url: Option<String>,
    tls: Option<TlsPaths>,
    web_enabled: bool,
) -> Result<(), Box<dyn Error>> {
    // MVP 默认只监听 127.0.0.1:8765；更复杂的配置文件/后台守护留给后续任务。
    let mut config = DaemonConfig::default();
    config.state_path = process_lock::anchor_state_path(&config.state_path)?;
    let _state_lock = process_lock::DaemonStateLock::acquire(&config.state_path)?;
    if let Some(listen) = listen {
        config.listen_host = listen.host;
        config.listen_port = listen.port;
    }
    let relay_endpoints =
        normalize_relay_endpoints(config.relay_endpoints.clone().into_iter().chain(relay_urls))?;
    if let Some(first_relay_endpoint) = relay_endpoints.first() {
        config.default_pairing_ws_url =
            RelayBaseUrl::parse(first_relay_endpoint)?.client_url_template();
    }
    let relay_proxy_url = resolve_relay_proxy_url(relay_proxy_url, &relay_endpoints, |name| {
        std::env::var(name).ok()
    })?;
    config.relay_daemon_token = relay_daemon_token.clone().map(SecretString::new);
    config.relay_endpoints = relay_endpoints.clone();
    config.relay_proxy_url = relay_proxy_url.clone();
    let protocol = try_default_protocol(config.clone())?;

    tracing::info!(
        host = %config.listen_host,
        port = config.listen_port,
        "starting termd HTTP/WebSocket daemon"
    );

    if !relay_endpoints.is_empty() {
        let relay_protocol = protocol.clone();
        let reconnect_policy = RelayReconnectPolicy::from_config(config.relay_reconnect);
        let _relay_tasks = spawn_relay_reconnect_supervisors(
            relay_endpoints,
            relay_daemon_token,
            relay_proxy_url,
            reconnect_policy,
            relay_protocol,
        );
    }
    serve_with_optional_tls(config, protocol, tls, web_enabled).await?;
    Ok(())
}

fn resolve_relay_proxy_url(
    cli_proxy_url: Option<String>,
    relay_endpoints: &[String],
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, CliError> {
    if let Some(cli_proxy_url) = cli_proxy_url {
        return normalize_relay_proxy_url(&cli_proxy_url)
            .map(Some)
            .map_err(|_| CliError::UnsupportedRelayProxy(cli_proxy_url));
    }

    for env_name in DEDICATED_RELAY_PROXY_ENV_VARS {
        let Some(value) = env_lookup(env_name) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        return normalize_relay_proxy_url(&value)
            .map(Some)
            .map_err(|_| CliError::UnsupportedRelayProxy(value));
    }

    // 通用代理变量只在实际配置 relay 时生效，避免用户系统里全局 HTTP_PROXY
    // 影响只跑本地 Web/PTY 的 daemon。
    let Some(relay_endpoint) = relay_endpoints.first() else {
        return Ok(None);
    };
    if relay_proxy_bypassed_by_no_proxy(relay_endpoint, &env_lookup) {
        return Ok(None);
    }

    for env_name in common_proxy_env_vars_for_relay(relay_endpoint) {
        let Some(value) = env_lookup(env_name) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        return normalize_relay_proxy_url(&value)
            .map(Some)
            .map_err(|_| CliError::UnsupportedRelayProxy(value));
    }

    Ok(None)
}

fn common_proxy_env_vars_for_relay(relay_endpoint: &str) -> &'static [&'static str] {
    if relay_endpoint.starts_with("wss://") {
        &COMMON_WSS_PROXY_ENV_VARS
    } else {
        &COMMON_WS_PROXY_ENV_VARS
    }
}

fn relay_proxy_bypassed_by_no_proxy(
    relay_endpoint: &str,
    env_lookup: &impl Fn(&str) -> Option<String>,
) -> bool {
    let Some((relay_host, relay_port)) = relay_host_port_for_proxy_env(relay_endpoint) else {
        return false;
    };

    for env_name in NO_PROXY_ENV_VARS {
        let Some(value) = env_lookup(env_name) else {
            continue;
        };
        if no_proxy_matches(&value, &relay_host, relay_port) {
            return true;
        }
    }
    false
}

fn relay_host_port_for_proxy_env(relay_endpoint: &str) -> Option<(String, u16)> {
    let (default_port, rest) = if let Some(rest) = relay_endpoint.strip_prefix("ws://") {
        (80, rest)
    } else {
        let rest = relay_endpoint.strip_prefix("wss://")?;
        (443, rest)
    };
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    parse_proxy_env_authority(authority, default_port)
}

fn parse_proxy_env_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, suffix) = after_bracket.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match suffix.strip_prefix(':') {
            Some(port) => port.parse::<u16>().ok()?,
            None if suffix.is_empty() => default_port,
            _ => return None,
        };
        return Some((host.to_owned(), port));
    }

    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return None;
        }
        return Some((host.to_owned(), port.parse::<u16>().ok()?));
    }

    if authority.is_empty() || authority.contains(':') {
        return None;
    }
    Some((authority.to_owned(), default_port))
}

fn no_proxy_matches(no_proxy: &str, relay_host: &str, relay_port: u16) -> bool {
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| no_proxy_entry_matches(entry, relay_host, relay_port))
}

fn no_proxy_entry_matches(entry: &str, relay_host: &str, relay_port: u16) -> bool {
    if entry == "*" {
        return true;
    }

    let (entry_host, entry_port) = split_no_proxy_entry(entry);
    if entry_port.is_some_and(|entry_port| entry_port != relay_port) {
        return false;
    }

    let entry_host = normalize_no_proxy_host(entry_host);
    let relay_host = normalize_no_proxy_host(relay_host);
    if entry_host.is_empty() || relay_host.is_empty() {
        return false;
    }
    if entry_host == relay_host {
        return true;
    }

    // 常见 NO_PROXY 语义中 `example.com` 和 `.example.com` 都匹配子域名。
    let suffix = entry_host.trim_start_matches('.');
    relay_host
        .strip_suffix(suffix)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

fn split_no_proxy_entry(entry: &str) -> (&str, Option<u16>) {
    if let Some(after_bracket) = entry.strip_prefix('[')
        && let Some((host, suffix)) = after_bracket.split_once(']')
    {
        return match suffix.strip_prefix(':') {
            Some(port) => (host, port.parse::<u16>().ok()),
            None => (host, None),
        };
    }

    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, port.parse::<u16>().ok()),
        _ => (entry, None),
    }
}

fn normalize_no_proxy_host(host: &str) -> String {
    host.trim().trim_matches('.').to_ascii_lowercase()
}

fn spawn_relay_reconnect_supervisors(
    relay_endpoints: Vec<String>,
    relay_daemon_token: Option<String>,
    relay_proxy_url: Option<String>,
    reconnect_policy: RelayReconnectPolicy,
    protocol: termd::net::server::SharedDaemonProtocol,
) -> Vec<JoinHandle<()>> {
    let relay_proxy = relay_proxy_url
        .as_deref()
        .and_then(|value| RelayProxyUrl::parse(value).ok());
    relay_endpoints
        .into_iter()
        .map(|relay_url| {
            let relay_protocol = protocol.clone();
            let relay_daemon_token = relay_daemon_token.clone();
            let relay_proxy = relay_proxy.clone();
            tokio::spawn(async move {
                // 目前 daemon 只允许配置一个 relay；保留 supervisor 边界，便于独立处理重连和心跳。
                if let Err(error) = run_relay_mux_with_reconnect(
                    &relay_url,
                    relay_daemon_token.as_deref(),
                    relay_proxy,
                    reconnect_policy,
                    relay_protocol,
                )
                .await
                {
                    tracing::warn!(%error, relay_url = %relay_url, "relay reconnect supervisor stopped");
                }
            })
        })
        .collect()
}

async fn serve_with_optional_tls(
    config: DaemonConfig,
    protocol: termd::net::server::SharedDaemonProtocol,
    tls: Option<TlsPaths>,
    web_enabled: bool,
) -> Result<(), termd::net::server::ServerError> {
    match tls {
        Some(tls) => serve_tls(config, protocol, tls, web_enabled).await,
        None => serve(config, protocol, web_enabled).await,
    }
}

#[derive(Clone, PartialEq, Eq)]
enum CliCommand {
    Help,
    Version,
    Serve {
        listen: Option<ListenAddress>,
        relay_urls: Vec<String>,
        relay_daemon_token: Option<String>,
        relay_proxy_url: Option<String>,
        tls: Option<TlsPaths>,
        web: bool,
    },
    Pair {
        url: String,
        qr: bool,
        qr_svg: Option<PathBuf>,
    },
    SessionSupervisor(SessionSupervisorArgs),
}

impl fmt::Debug for CliCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Help => formatter.write_str("Help"),
            Self::Version => formatter.write_str("Version"),
            Self::Serve {
                listen,
                relay_urls,
                relay_daemon_token,
                relay_proxy_url,
                tls,
                web,
            } => formatter
                .debug_struct("Serve")
                .field("listen", listen)
                .field("relay_urls", relay_urls)
                .field(
                    "relay_daemon_token_configured",
                    &relay_daemon_token.is_some(),
                )
                .field("relay_proxy_url", relay_proxy_url)
                .field("tls", tls)
                .field("web", web)
                .finish(),
            Self::Pair { url, qr, qr_svg } => formatter
                .debug_struct("Pair")
                .field("url", url)
                .field("qr", qr)
                .field("qr_svg", qr_svg)
                .finish(),
            Self::SessionSupervisor(args) => formatter
                .debug_tuple("SessionSupervisor")
                .field(args)
                .finish(),
        }
    }
}

impl CliCommand {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().peekable();
        let Some(command) = args.next() else {
            return Ok(Self::Serve {
                listen: None,
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            });
        };

        match command.as_str() {
            "-h" | "--help" | "help" => Ok(Self::Help),
            "-V" | "--version" | "version" => Ok(Self::Version),
            "pair" => parse_pair_args(args),
            "__session-supervisor" => parse_session_supervisor_args(args),
            "--listen"
            | "--relay"
            | "--relay-url"
            | "--relay-daemon-token"
            | "--relay-daemon-token-file"
            | "--relay-proxy"
            | "--tls-cert"
            | "--tls-key"
            | "--web" => parse_serve_args(std::iter::once(command).chain(args)),
            other => Err(CliError::UnknownCommand(other.to_owned())),
        }
    }
}

fn parse_serve_args(args: impl IntoIterator<Item = String>) -> Result<CliCommand, CliError> {
    let mut listen = None;
    let mut relay_urls = Vec::new();
    let mut relay_daemon_token = None;
    let mut relay_proxy_url = None;
    let mut tls_cert = None;
    let mut tls_key = None;
    let mut web = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliCommand::Help),
            "-V" | "--version" => return Ok(CliCommand::Version),
            "--listen" => {
                let value = args.next().ok_or(CliError::MissingListenValue)?;
                listen = Some(parse_listen_address(&value)?);
            }
            "--relay" | "--relay-url" => {
                let value = args.next().ok_or(CliError::MissingRelayUrlValue)?;
                let _ = RelayBaseUrl::parse(&value)
                    .map_err(|_| CliError::UnsupportedRelayUrl(value.clone()))?;
                if !relay_urls.is_empty() {
                    return Err(CliError::TooManyRelayUrls);
                }
                relay_urls.push(value);
            }
            "--relay-daemon-token" => {
                if relay_daemon_token.is_some() {
                    return Err(CliError::ConflictingRelayDaemonTokenSources);
                }
                let value = args.next().ok_or(CliError::MissingRelayDaemonTokenValue)?;
                if value.is_empty() {
                    return Err(CliError::EmptyRelayDaemonTokenValue);
                }
                relay_daemon_token = Some(value);
            }
            "--relay-daemon-token-file" => {
                if relay_daemon_token.is_some() {
                    return Err(CliError::ConflictingRelayDaemonTokenSources);
                }
                let value = args
                    .next()
                    .ok_or(CliError::MissingRelayDaemonTokenFileValue)?;
                relay_daemon_token = Some(read_secret_file(
                    "--relay-daemon-token-file",
                    &value,
                    CliError::ReadRelayDaemonTokenFile,
                )?);
            }
            "--relay-proxy" => {
                let value = args.next().ok_or(CliError::MissingRelayProxyValue)?;
                let proxy = normalize_relay_proxy_url(&value)
                    .map_err(|_| CliError::UnsupportedRelayProxy(value.clone()))?;
                relay_proxy_url = Some(proxy);
            }
            "--tls-cert" => {
                let value = args.next().ok_or(CliError::MissingTlsCertValue)?;
                if value.is_empty() {
                    return Err(CliError::EmptyTlsCertValue);
                }
                tls_cert = Some(PathBuf::from(value));
            }
            "--tls-key" => {
                let value = args.next().ok_or(CliError::MissingTlsKeyValue)?;
                if value.is_empty() {
                    return Err(CliError::EmptyTlsKeyValue);
                }
                tls_key = Some(PathBuf::from(value));
            }
            "--web" => {
                web = true;
            }
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
    }

    let tls = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(TlsPaths::new(cert, key)),
        (None, None) => None,
        _ => return Err(CliError::IncompleteTlsConfig),
    };

    Ok(CliCommand::Serve {
        listen,
        relay_urls,
        relay_daemon_token,
        relay_proxy_url,
        tls,
        web,
    })
}

fn read_secret_file(
    flag: &'static str,
    path: &str,
    read_error: CliError,
) -> Result<String, CliError> {
    if path.is_empty() {
        return Err(CliError::EmptySecretFilePath(flag));
    }
    let token = fs::read_to_string(path).map_err(|_| read_error)?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(CliError::EmptySecretFile(flag));
    }
    Ok(token)
}

fn parse_pair_args(mut args: impl Iterator<Item = String>) -> Result<CliCommand, CliError> {
    let mut url = DEFAULT_PAIRING_URL.to_owned();
    let mut qr = false;
    let mut qr_svg = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliCommand::Help),
            "-V" | "--version" => return Ok(CliCommand::Version),
            "--url" => {
                url = args.next().ok_or(CliError::MissingUrlValue)?;
            }
            "--qr" => {
                qr = true;
            }
            "--qr-svg" => {
                qr_svg = Some(PathBuf::from(
                    args.next().ok_or(CliError::MissingQrSvgPath)?,
                ));
            }
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
    }

    // 解析阶段先拒绝不支持的 URL，避免用户等到网络请求时才看到模糊错误。
    let _ = parse_pairing_base_url(&url)?;
    Ok(CliCommand::Pair { url, qr, qr_svg })
}

fn parse_session_supervisor_args(
    mut args: impl Iterator<Item = String>,
) -> Result<CliCommand, CliError> {
    let mut session_id = None;
    let mut socket_path = None;
    let mut command = None;
    let mut size = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session-id" => {
                session_id = Some(args.next().ok_or(CliError::MissingSupervisorSessionId)?);
            }
            "--socket-path" => {
                let value = args.next().ok_or(CliError::MissingSupervisorSocketPath)?;
                socket_path = Some(PathBuf::from(value));
            }
            "--command-base64" => {
                let value = args.next().ok_or(CliError::MissingSupervisorCommand)?;
                let decoded = general_purpose::STANDARD
                    .decode(value)
                    .map_err(|_| CliError::InvalidSupervisorPayload)?;
                command = Some(
                    serde_json::from_slice::<CommandSpec>(&decoded)
                        .map_err(|_| CliError::InvalidSupervisorPayload)?,
                );
            }
            "--size-base64" => {
                let value = args.next().ok_or(CliError::MissingSupervisorSize)?;
                let decoded = general_purpose::STANDARD
                    .decode(value)
                    .map_err(|_| CliError::InvalidSupervisorPayload)?;
                size = Some(
                    serde_json::from_slice::<PtySize>(&decoded)
                        .map_err(|_| CliError::InvalidSupervisorPayload)?,
                );
            }
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(CliCommand::SessionSupervisor(SessionSupervisorArgs {
        session_id: session_id.ok_or(CliError::MissingSupervisorSessionId)?,
        socket_path: socket_path.ok_or(CliError::MissingSupervisorSocketPath)?,
        command: command.ok_or(CliError::MissingSupervisorCommand)?,
        size: size.ok_or(CliError::MissingSupervisorSize)?,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenAddress {
    host: String,
    port: u16,
}

fn parse_listen_address(value: &str) -> Result<ListenAddress, CliError> {
    if value.trim() != value || value.is_empty() {
        return Err(CliError::InvalidListenAddress(value.to_owned()));
    }

    let addr = value
        .parse::<SocketAddr>()
        .map_err(|_| CliError::InvalidListenAddress(value.to_owned()))?;

    Ok(ListenAddress {
        host: addr.ip().to_string(),
        port: addr.port(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PairingBaseUrl {
    scheme: PairingUrlScheme,
    host: String,
    port: u16,
    authority: String,
}

#[derive(Debug, Deserialize)]
struct PairingTokenResponse {
    token: PairingToken,
    expires_at_ms: UnixTimestampMillis,
    server_id: ServerId,
    daemon_public_key: PublicKey,
}

fn parse_pairing_base_url(url: &str) -> Result<PairingBaseUrl, CliError> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("http://") {
        (PairingUrlScheme::Http, rest)
    } else if let Some(rest) = url.strip_prefix("https://") {
        (PairingUrlScheme::Https, rest)
    } else {
        return Err(CliError::UnsupportedUrl(url.to_owned()));
    };
    if rest.is_empty() || rest.contains('?') || rest.contains('#') {
        return Err(CliError::InvalidUrl(url.to_owned()));
    }

    let authority = match rest.split_once('/') {
        Some((authority, "")) => authority,
        Some(_) => return Err(CliError::UnsupportedUrl(url.to_owned())),
        None => rest,
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(CliError::InvalidUrl(url.to_owned()));
    }

    let (host, port) = parse_host_port_authority(authority, url)?;
    Ok(PairingBaseUrl {
        scheme,
        host,
        port,
        authority: authority.to_owned(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingUrlScheme {
    Http,
    Https,
}

fn parse_host_port_authority(
    authority: &str,
    original_url: &str,
) -> Result<(String, u16), CliError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((host, port)) = after_bracket.split_once("]:") else {
            return Err(CliError::InvalidUrl(original_url.to_owned()));
        };
        let Ok(port) = port.parse::<u16>() else {
            return Err(CliError::InvalidUrl(original_url.to_owned()));
        };
        if host.is_empty() {
            return Err(CliError::InvalidUrl(original_url.to_owned()));
        }
        return Ok((host.to_owned(), port));
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Err(CliError::InvalidUrl(original_url.to_owned()));
    };
    let Ok(port) = port.parse::<u16>() else {
        return Err(CliError::InvalidUrl(original_url.to_owned()));
    };
    if host.is_empty() {
        return Err(CliError::InvalidUrl(original_url.to_owned()));
    }
    Ok((host.to_owned(), port))
}

fn request_pairing_token_response(base_url: &str) -> Result<PairingTokenResponse, CliError> {
    let endpoint = parse_pairing_base_url(base_url)?;
    match endpoint.scheme {
        PairingUrlScheme::Http => request_pairing_token_over_http(&endpoint),
        PairingUrlScheme::Https => request_pairing_token_over_https(&endpoint),
    }
}

fn request_pairing_token_over_http(
    endpoint: &PairingBaseUrl,
) -> Result<PairingTokenResponse, CliError> {
    let mut stream = connect_pairing_tcp(endpoint)?;
    stream
        .set_read_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;
    stream
        .set_write_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;

    write_pairing_http_request(&mut stream, endpoint)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|_| CliError::ReceiveFailed)?;
    parse_pairing_token_http_response(&response)
}

fn request_pairing_token_over_https(
    endpoint: &PairingBaseUrl,
) -> Result<PairingTokenResponse, CliError> {
    let tcp = connect_pairing_tcp(endpoint)?;
    tcp.set_read_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;
    tcp.set_write_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(endpoint.host.clone())
        .map_err(|_| CliError::InvalidUrl(endpoint.authority.clone()))?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|_| CliError::Tls)?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    write_pairing_http_request(&mut stream, endpoint)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|_| CliError::ReceiveFailed)?;
    parse_pairing_token_http_response(&response)
}

fn connect_pairing_tcp(endpoint: &PairingBaseUrl) -> Result<TcpStream, CliError> {
    (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|_| CliError::InvalidUrl(endpoint.authority.clone()))?
        .next()
        .ok_or_else(|| CliError::InvalidUrl(endpoint.authority.clone()))
        .and_then(|addr| TcpStream::connect(addr).map_err(|_| CliError::ConnectFailed))
}

fn write_pairing_http_request(
    stream: &mut impl Write,
    endpoint: &PairingBaseUrl,
) -> Result<(), CliError> {
    // 本地管理端点只签发 token，不携带请求体，避免把敏感材料写入日志或 shell history。
    let request = format!(
        "POST {LOCAL_PAIRING_TOKEN_PATH} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        endpoint.authority
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| CliError::SendFailed)
}

fn parse_pairing_token_http_response(response: &[u8]) -> Result<PairingTokenResponse, CliError> {
    let raw = std::str::from_utf8(response).map_err(|_| CliError::InvalidHttpResponse)?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or(CliError::InvalidHttpResponse)?;
    let status = parse_http_status(head)?;
    if status != 200 {
        return Err(CliError::HttpStatus { status });
    }

    let payload: PairingTokenResponse =
        serde_json::from_str(body).map_err(|_| CliError::InvalidJson)?;
    if payload.token.0.is_empty() {
        return Err(CliError::InvalidJson);
    }
    Ok(payload)
}

/// 生成 QR invite 时只保留短期信任材料；连接地址由 Web 当前页面决定。
fn build_pairing_qr_payload(token: &PairingTokenResponse) -> Result<PairingQrPayload, CliError> {
    Ok(
        PairingQrPayload::new(token.token.clone(), token.server_id, token.expires_at_ms)
            .with_daemon_public_key(token.daemon_public_key.clone()),
    )
}

fn render_pairing_qr(invite_code: &str) -> Result<String, CliError> {
    let code = QrCode::new(invite_code.as_bytes()).map_err(|_| CliError::QrRenderFailed)?;

    // 终端输出用 Unicode 二维码；邀请码会在下一行单独打印，便于复制粘贴。
    Ok(code.render::<unicode::Dense1x2>().build())
}

fn write_pairing_qr_svg(path: &std::path::Path, invite_code: &str) -> Result<(), CliError> {
    let svg = render_pairing_qr_svg(invite_code)?;
    std::fs::write(path, svg).map_err(|_| CliError::QrSvgWriteFailed)
}

fn render_pairing_qr_svg(invite_code: &str) -> Result<String, CliError> {
    let code = QrCode::new(invite_code.as_bytes()).map_err(|_| CliError::QrRenderFailed)?;
    let modules = code.width();
    let quiet_zone = 4_usize;
    let side = modules + quiet_zone * 2;
    let mut path = String::new();

    for (index, color) in code.to_colors().iter().enumerate() {
        if *color != QrColor::Dark {
            continue;
        }
        let x = index % modules + quiet_zone;
        let y = index / modules + quiet_zone;
        // SVG 用 path 合并黑色模块，文件体积比逐个 rect 更小，扫码器也更容易识别。
        write!(&mut path, "M{x} {y}h1v1H{x}V{y}").map_err(|_| CliError::QrRenderFailed)?;
    }

    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" version="1.1" width="{side}" height="{side}" viewBox="0 0 {side} {side}" shape-rendering="crispEdges"><rect x="0" y="0" width="{side}" height="{side}" fill="#ffffff"/><path d="{path}" fill="#000000"/></svg>"##
    ))
}

fn parse_http_status(head: &str) -> Result<u16, CliError> {
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or(CliError::InvalidHttpResponse)?;
    status
        .parse::<u16>()
        .map_err(|_| CliError::InvalidHttpResponse)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliError {
    UnknownCommand(String),
    UnexpectedArgument(String),
    MissingListenValue,
    MissingUrlValue,
    MissingQrSvgPath,
    MissingRelayUrlValue,
    MissingRelayDaemonTokenValue,
    MissingRelayDaemonTokenFileValue,
    MissingRelayProxyValue,
    MissingTlsCertValue,
    MissingTlsKeyValue,
    EmptyRelayDaemonTokenValue,
    EmptySecretFilePath(&'static str),
    EmptySecretFile(&'static str),
    ReadRelayDaemonTokenFile,
    ConflictingRelayDaemonTokenSources,
    EmptyTlsCertValue,
    EmptyTlsKeyValue,
    TooManyRelayUrls,
    IncompleteTlsConfig,
    UnsupportedUrl(String),
    UnsupportedRelayUrl(String),
    UnsupportedRelayProxy(String),
    InvalidListenAddress(String),
    InvalidUrl(String),
    ConnectFailed,
    SendFailed,
    ReceiveFailed,
    LocalIo,
    InvalidHttpResponse,
    HttpStatus { status: u16 },
    InvalidJson,
    QrRenderFailed,
    QrSvgWriteFailed,
    Tls,
    MissingSupervisorSessionId,
    MissingSupervisorSocketPath,
    MissingSupervisorCommand,
    MissingSupervisorSize,
    InvalidSupervisorPayload,
}

impl CliError {
    fn usage() -> &'static str {
        "usage: termd [--listen 127.0.0.1:8765] [--relay ws://host:port] [--relay-daemon-token <token>|--relay-daemon-token-file <path>] [--relay-proxy http://host:port|socks5://host:port] [--tls-cert <cert.pem> --tls-key <key.pem>] [--web] [pair [--url http://127.0.0.1:8765|https://127.0.0.1:8765] [--qr] [--qr-svg <path>]]\ntry `termd --help` for full help"
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(_command) => {
                write!(f, "unknown termd command\n{}", Self::usage())
            }
            Self::UnexpectedArgument(_argument) => {
                write!(f, "unexpected argument\n{}", Self::usage())
            }
            Self::MissingListenValue => write!(f, "`--listen` requires a value\n{}", Self::usage()),
            Self::MissingUrlValue => write!(f, "`--url` requires a value\n{}", Self::usage()),
            Self::MissingQrSvgPath => write!(f, "`--qr-svg` requires a path\n{}", Self::usage()),
            Self::MissingRelayUrlValue => {
                write!(f, "`--relay` requires a value\n{}", Self::usage())
            }
            Self::MissingRelayDaemonTokenValue => {
                write!(
                    f,
                    "`--relay-daemon-token` requires a value\n{}",
                    Self::usage()
                )
            }
            Self::MissingRelayDaemonTokenFileValue => {
                write!(
                    f,
                    "`--relay-daemon-token-file` requires a value\n{}",
                    Self::usage()
                )
            }
            Self::MissingRelayProxyValue => {
                write!(f, "`--relay-proxy` requires a value\n{}", Self::usage())
            }
            Self::MissingTlsCertValue => {
                write!(f, "`--tls-cert` requires a value\n{}", Self::usage())
            }
            Self::MissingTlsKeyValue => {
                write!(f, "`--tls-key` requires a value\n{}", Self::usage())
            }
            Self::EmptyRelayDaemonTokenValue => {
                write!(
                    f,
                    "`--relay-daemon-token` requires a non-empty value\n{}",
                    Self::usage()
                )
            }
            Self::EmptySecretFilePath(flag) => {
                write!(f, "`{flag}` requires a non-empty path\n{}", Self::usage())
            }
            Self::EmptySecretFile(flag) => write!(f, "`{flag}` points to an empty secret file"),
            Self::ReadRelayDaemonTokenFile => {
                write!(f, "failed to read relay daemon token file")
            }
            Self::ConflictingRelayDaemonTokenSources => write!(
                f,
                "`--relay-daemon-token` and `--relay-daemon-token-file` cannot be used together\n{}",
                Self::usage()
            ),
            Self::EmptyTlsCertValue => {
                write!(
                    f,
                    "`--tls-cert` requires a non-empty value\n{}",
                    Self::usage()
                )
            }
            Self::EmptyTlsKeyValue => {
                write!(
                    f,
                    "`--tls-key` requires a non-empty value\n{}",
                    Self::usage()
                )
            }
            Self::TooManyRelayUrls => {
                write!(
                    f,
                    "a daemon can connect to only one relay; configure a single `--relay`\n{}",
                    Self::usage()
                )
            }
            Self::IncompleteTlsConfig => {
                write!(
                    f,
                    "`--tls-cert` and `--tls-key` must be configured together\n{}",
                    Self::usage()
                )
            }
            Self::UnsupportedUrl(_url) => write!(
                f,
                "unsupported daemon URL; expected http://host:port or https://host:port"
            ),
            Self::UnsupportedRelayUrl(_url) => {
                write!(
                    f,
                    "unsupported relay URL; expected ws://host:port or wss://host:port"
                )
            }
            Self::UnsupportedRelayProxy(_url) => {
                write!(
                    f,
                    "unsupported relay proxy; expected http://host:port or socks5://host:port"
                )
            }
            Self::InvalidListenAddress(address) => {
                write!(
                    f,
                    "invalid listen address `{address}`; expected host:port such as 127.0.0.1:8765"
                )
            }
            Self::InvalidUrl(_url) => {
                write!(
                    f,
                    "invalid daemon URL; expected http://host:port or https://host:port"
                )
            }
            Self::ConnectFailed => write!(f, "failed to connect to the running termd daemon"),
            Self::SendFailed => write!(f, "failed to send pairing token request"),
            Self::ReceiveFailed => write!(f, "failed to read pairing token response"),
            Self::LocalIo => write!(f, "local IO error while requesting pairing token"),
            Self::InvalidHttpResponse => write!(f, "daemon returned an invalid HTTP response"),
            Self::HttpStatus { status } => {
                write!(
                    f,
                    "daemon returned HTTP {status} while issuing pairing token"
                )
            }
            Self::InvalidJson => write!(f, "daemon returned an invalid pairing token response"),
            Self::QrRenderFailed => write!(f, "failed to render pairing QR code"),
            Self::QrSvgWriteFailed => write!(f, "failed to write pairing QR SVG"),
            Self::Tls => write!(f, "failed to establish TLS for pairing token request"),
            Self::MissingSupervisorSessionId => write!(f, "`--session-id` requires a value"),
            Self::MissingSupervisorSocketPath => write!(f, "`--socket-path` requires a value"),
            Self::MissingSupervisorCommand => write!(f, "`--command-base64` requires a value"),
            Self::MissingSupervisorSize => write!(f, "`--size-base64` requires a value"),
            Self::InvalidSupervisorPayload => {
                write!(f, "session supervisor payload is invalid")
            }
        }
    }
}

impl Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_args_as_serve() {
        assert_eq!(
            CliCommand::parse(std::iter::empty::<String>()).unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn parses_help_and_version_without_starting_server() {
        assert_eq!(
            CliCommand::parse(["--help".to_owned()]).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            CliCommand::parse(["-h".to_owned()]).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            CliCommand::parse(["--version".to_owned()]).unwrap(),
            CliCommand::Version
        );
        assert_eq!(
            CliCommand::parse(["pair".to_owned(), "--help".to_owned()]).unwrap(),
            CliCommand::Help
        );
        assert!(HELP_TEXT.contains("termd install [OPTIONS]"));
        assert!(HELP_TEXT.contains("termd uninstall [OPTIONS]"));
    }

    #[test]
    fn recognizes_install_and_uninstall_before_daemon_argument_parsing() {
        assert!(matches!(
            installer_options(["termd", "install", "--help"]),
            Some(terminstall::Options::Install(_))
        ));
        assert!(matches!(
            installer_options(["termd", "uninstall", "--dry-run"]),
            Some(terminstall::Options::Uninstall(_))
        ));
        assert!(installer_options(["termd", "pair", "--help"]).is_none());
    }

    #[test]
    fn help_and_parser_do_not_expose_legacy_relay_auth_flags() {
        assert!(!HELP_TEXT.contains("--relay-auth-token"));
        assert!(!CliError::usage().contains("--relay-auth-token"));

        for flag in ["--relay-auth-token", "--relay-auth-token-file"] {
            assert!(matches!(
                CliCommand::parse([
                    "--relay".to_owned(),
                    "ws://127.0.0.1:8080".to_owned(),
                    flag.to_owned(),
                    "legacy-secret".to_owned(),
                ])
                .unwrap_err(),
                CliError::UnexpectedArgument(argument) if argument == flag
            ));
        }
    }

    #[test]
    fn parses_listen_address_for_serve() {
        assert_eq!(
            CliCommand::parse(["--listen".to_owned(), "0.0.0.0:8765".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: Some(ListenAddress {
                    host: "0.0.0.0".to_owned(),
                    port: 8765,
                }),
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn parses_ipv6_listen_address_for_serve() {
        assert_eq!(
            CliCommand::parse(["--listen".to_owned(), "[::1]:8765".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: Some(ListenAddress {
                    host: "::1".to_owned(),
                    port: 8765,
                }),
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn parses_relay_url_for_serve() {
        assert_eq!(
            CliCommand::parse(["--relay".to_owned(), "ws://127.0.0.1:8080/".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["ws://127.0.0.1:8080/".to_owned()],
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
        assert_eq!(
            CliCommand::parse(["--relay-url".to_owned(), "ws://127.0.0.1:8080".to_owned()])
                .unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["ws://127.0.0.1:8080".to_owned()],
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn parses_wss_relay_url_for_serve() {
        assert_eq!(
            CliCommand::parse(["--relay".to_owned(), "wss://termd.yiln.de/ws".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["wss://termd.yiln.de/ws".to_owned()],
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn rejects_multiple_relay_urls_for_serve() {
        assert!(matches!(
            CliCommand::parse([
                "--relay".to_owned(),
                "ws://127.0.0.1:8080".to_owned(),
                "--relay-url".to_owned(),
                "wss://relay.example:443".to_owned(),
            ])
            .unwrap_err(),
            CliError::TooManyRelayUrls
        ));
    }

    #[test]
    fn parses_web_flag_for_serve() {
        assert_eq!(
            CliCommand::parse(["--web".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: None,
                web: true,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_relay_reconnect_supervisors_keeps_endpoints_independent() {
        use axum::extract::State;
        use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use futures_util::StreamExt;
        use serde_json::Value;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use termd::config::RelayReconnectConfig;
        use termd_proto::{Envelope, MessageType, RouteHelloPayload, RouteReadyPayload, RouteRole};

        #[derive(Clone, Default)]
        struct MockMuxState {
            attempts: Arc<AtomicUsize>,
            control_attempts: Arc<AtomicUsize>,
            data_attempts: Arc<AtomicUsize>,
            early_closes: Arc<AtomicUsize>,
            idle_pings: Arc<AtomicUsize>,
            route_ready_sent: Arc<AtomicUsize>,
            close_first_attempt: bool,
        }

        async fn mock_daemon_mux_ws(
            websocket: WebSocketUpgrade,
            State(state): State<MockMuxState>,
        ) -> impl IntoResponse {
            websocket.on_upgrade(move |mut socket| async move {
                let attempt = state.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if state.close_first_attempt && attempt == 1 {
                    // 故意让这个 relay 首连失败，验证另一个 endpoint 仍然可以独立存活。
                    state.early_closes.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                let Some(route_hello) = read_route_hello(&mut socket).await else {
                    return;
                };
                match route_hello.role {
                    RouteRole::DaemonControl => {
                        state.control_attempts.fetch_add(1, Ordering::SeqCst);
                    }
                    RouteRole::DaemonData => {
                        // 中文注释：data 连接只能由 client 触发的 OpenData 按需创建；
                        // 这个多 endpoint 稳定性测试没有模拟 client，因此计数应保持 0。
                        state.data_attempts.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => return,
                }
                let route_ready = Envelope::new(
                    MessageType::RouteReady,
                    RouteReadyPayload {
                        server_id: route_hello.server_id,
                        role: route_hello.role,
                    },
                );
                let raw = serde_json::to_string(&route_ready).unwrap();
                if socket.send(AxumMessage::Text(raw)).await.is_err() {
                    return;
                }
                state.route_ready_sent.fetch_add(1, Ordering::SeqCst);

                while let Some(message) = socket.next().await {
                    match message {
                        Ok(AxumMessage::Binary(_)) => {}
                        Ok(AxumMessage::Ping(payload)) => {
                            state.idle_pings.fetch_add(1, Ordering::SeqCst);
                            let _ = socket.send(AxumMessage::Pong(payload)).await;
                        }
                        Ok(AxumMessage::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            })
        }

        async fn read_route_hello(
            socket: &mut axum::extract::ws::WebSocket,
        ) -> Option<RouteHelloPayload> {
            loop {
                let message = socket.next().await?.ok()?;
                match message {
                    AxumMessage::Text(raw) => {
                        let envelope: Envelope<Value> = serde_json::from_str(raw.as_str()).ok()?;
                        if envelope.kind != MessageType::RouteHello {
                            return None;
                        }
                        return serde_json::from_value(envelope.payload).ok();
                    }
                    AxumMessage::Binary(raw) => {
                        let envelope: Envelope<Value> = serde_json::from_slice(&raw).ok()?;
                        if envelope.kind != MessageType::RouteHello {
                            return None;
                        }
                        return serde_json::from_value(envelope.payload).ok();
                    }
                    AxumMessage::Ping(payload) => {
                        let _ = socket.send(AxumMessage::Pong(payload)).await;
                    }
                    AxumMessage::Pong(_) => {}
                    AxumMessage::Close(_) => return None,
                }
            }
        }

        let flaky_state = MockMuxState {
            close_first_attempt: true,
            ..MockMuxState::default()
        };
        let healthy_state = MockMuxState::default();

        let flaky_app = axum::Router::new()
            .route("/ws", get(mock_daemon_mux_ws))
            .with_state(flaky_state.clone());
        let healthy_app = axum::Router::new()
            .route("/ws", get(mock_daemon_mux_ws))
            .with_state(healthy_state.clone());

        let flaky_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let flaky_addr = flaky_listener.local_addr().unwrap();
        let healthy_addr = healthy_listener.local_addr().unwrap();

        let flaky_server = tokio::spawn(async move {
            axum::serve(flaky_listener, flaky_app).await.unwrap();
        });
        let healthy_server = tokio::spawn(async move {
            axum::serve(healthy_listener, healthy_app).await.unwrap();
        });

        let reconnect_policy = RelayReconnectPolicy::from_config(RelayReconnectConfig {
            initial_delay_ms: 10,
            max_delay_ms: 20,
            heartbeat_interval_ms: 10,
        });
        let state_dir = std::env::temp_dir().join(format!(
            "termd-main-test-{}-{}-relay-state",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&state_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let protocol = termd::net::server::default_protocol(DaemonConfig::default_for_state_path(
            state_dir.join("state.json"),
        ));
        let relay_tasks = spawn_relay_reconnect_supervisors(
            vec![format!("ws://{flaky_addr}"), format!("ws://{healthy_addr}")],
            None,
            None,
            reconnect_policy,
            protocol,
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if flaky_state.attempts.load(Ordering::SeqCst) >= 2
                    && healthy_state.route_ready_sent.load(Ordering::SeqCst) >= 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert!(
            healthy_state.idle_pings.load(Ordering::SeqCst) >= 1,
            "健康 relay supervisor 空闲时应发送 WebSocket Ping，避免公网代理清理静默主干"
        );
        assert_eq!(
            healthy_state.control_attempts.load(Ordering::SeqCst),
            1,
            "健康 relay supervisor 应只建立一条稳定 control 连接，不能被其他 endpoint 的重连牵连"
        );
        assert_eq!(
            healthy_state.data_attempts.load(Ordering::SeqCst),
            0,
            "没有 client 的健康 relay 不应主动预热 data 连接"
        );
        assert_eq!(
            flaky_state.early_closes.load(Ordering::SeqCst),
            1,
            "故障 relay endpoint 仍应按退避独立重连一次"
        );
        assert_eq!(
            flaky_state.control_attempts.load(Ordering::SeqCst),
            1,
            "故障 relay endpoint 重连后应只留下自己的稳定 control 连接"
        );

        for handle in relay_tasks {
            handle.abort();
            let _ = handle.await;
        }
        flaky_server.abort();
        healthy_server.abort();
        let _ = flaky_server.await;
        let _ = healthy_server.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn parses_tls_cert_and_key_for_serve_without_key_path_debug_leakage() {
        let command = CliCommand::parse([
            "--tls-cert".to_owned(),
            "/etc/termd/fullchain.pem".to_owned(),
            "--tls-key".to_owned(),
            "/etc/termd/secret-key.pem".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Serve {
                listen: None,
                relay_urls: Vec::new(),
                relay_daemon_token: None,
                relay_proxy_url: None,
                tls: Some(TlsPaths::new(
                    "/etc/termd/fullchain.pem",
                    "/etc/termd/secret-key.pem"
                )),
                web: false,
            }
        );
        assert!(!format!("{command:?}").contains("secret-key.pem"));
    }

    #[test]
    fn parses_relay_daemon_token_file_for_serve_without_debug_leakage() {
        let daemon_path = std::env::temp_dir().join(format!(
            "termd-relay-daemon-token-{}.txt",
            std::process::id()
        ));
        std::fs::write(&daemon_path, "daemon-secret-from-file\n").unwrap();

        let command = CliCommand::parse([
            "--relay".to_owned(),
            "ws://127.0.0.1:8080".to_owned(),
            "--relay-daemon-token-file".to_owned(),
            daemon_path.to_string_lossy().into_owned(),
        ])
        .unwrap();

        std::fs::remove_file(&daemon_path).ok();
        assert_eq!(
            command,
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["ws://127.0.0.1:8080".to_owned()],
                relay_daemon_token: Some("daemon-secret-from-file".to_owned()),
                relay_proxy_url: None,
                tls: None,
                web: false,
            }
        );
        let rendered = format!("{command:?}");
        assert!(!rendered.contains("daemon-secret-from-file"));
    }

    #[test]
    fn parses_relay_proxy_for_serve() {
        assert_eq!(
            CliCommand::parse([
                "--relay".to_owned(),
                "wss://relay.example/ws".to_owned(),
                "--relay-proxy".to_owned(),
                " http://127.0.0.1:3128/ ".to_owned(),
            ])
            .unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["wss://relay.example/ws".to_owned()],
                relay_daemon_token: None,
                relay_proxy_url: Some("http://127.0.0.1:3128".to_owned()),
                tls: None,
                web: false,
            }
        );

        assert_eq!(
            CliCommand::parse([
                "--relay".to_owned(),
                "ws://127.0.0.1:8080".to_owned(),
                "--relay-proxy".to_owned(),
                "socks5://127.0.0.1:1080".to_owned(),
            ])
            .unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["ws://127.0.0.1:8080".to_owned()],
                relay_daemon_token: None,
                relay_proxy_url: Some("socks5://127.0.0.1:1080".to_owned()),
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn rejects_unsupported_relay_proxy_for_serve() {
        assert!(matches!(
            CliCommand::parse([
                "--relay".to_owned(),
                "ws://127.0.0.1:8080".to_owned(),
                "--relay-proxy".to_owned(),
                "https://proxy.example:443".to_owned(),
            ])
            .unwrap_err(),
            CliError::UnsupportedRelayProxy(url) if url == "https://proxy.example:443"
        ));
    }

    #[test]
    fn resolves_relay_proxy_from_env_when_cli_is_absent() {
        let resolved = resolve_relay_proxy_url(None, &[], |name| match name {
            "TERMD_RELAY_PROXY_URL" => Some(" socks5://127.0.0.1:1080/ ".to_owned()),
            _ => None,
        })
        .unwrap();

        assert_eq!(resolved, Some("socks5://127.0.0.1:1080".to_owned()));
    }

    #[test]
    fn resolves_relay_proxy_from_common_http_proxy_env() {
        let resolved = resolve_relay_proxy_url(
            None,
            &["wss://relay.example/ws".to_owned()],
            |name| match name {
                "HTTP_PROXY" => Some(" http://127.0.0.1:3128/ ".to_owned()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(resolved, Some("http://127.0.0.1:3128".to_owned()));
    }

    #[test]
    fn resolves_relay_proxy_from_all_proxy_env() {
        let resolved = resolve_relay_proxy_url(
            None,
            &["ws://relay.example/ws".to_owned()],
            |name| match name {
                "ALL_PROXY" => Some("socks5://127.0.0.1:1080".to_owned()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(resolved, Some("socks5://127.0.0.1:1080".to_owned()));
    }

    #[test]
    fn dedicated_relay_proxy_env_overrides_common_proxy_env() {
        let resolved = resolve_relay_proxy_url(
            None,
            &["wss://relay.example/ws".to_owned()],
            |name| match name {
                "TERMD_RELAY_PROXY" => Some("socks5://127.0.0.1:1080".to_owned()),
                "HTTP_PROXY" => Some("http://127.0.0.1:3128".to_owned()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(resolved, Some("socks5://127.0.0.1:1080".to_owned()));
    }

    #[test]
    fn secure_relay_prefers_https_proxy_before_http_proxy() {
        let resolved = resolve_relay_proxy_url(
            None,
            &["wss://relay.example/ws".to_owned()],
            |name| match name {
                "HTTPS_PROXY" => Some("http://127.0.0.1:9443".to_owned()),
                "HTTP_PROXY" => Some("http://127.0.0.1:3128".to_owned()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(resolved, Some("http://127.0.0.1:9443".to_owned()));
    }

    #[test]
    fn no_proxy_bypasses_common_proxy_env_for_matching_relay_host() {
        let resolved = resolve_relay_proxy_url(
            None,
            &["wss://relay.example/ws".to_owned()],
            |name| match name {
                "NO_PROXY" => Some("localhost,.example".to_owned()),
                "HTTPS_PROXY" => Some("http://127.0.0.1:9443".to_owned()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(resolved, None);
    }

    #[test]
    fn relay_proxy_cli_value_overrides_env() {
        let resolved =
            resolve_relay_proxy_url(Some("http://127.0.0.1:3128".to_owned()), &[], |_| {
                Some("socks5://127.0.0.1:1080".to_owned())
            })
            .unwrap();

        assert_eq!(resolved, Some("http://127.0.0.1:3128".to_owned()));
    }

    #[test]
    fn invalid_relay_proxy_env_is_rejected() {
        assert!(matches!(
            resolve_relay_proxy_url(
                None,
                &[],
                |name| match name {
                    "TERMD_RELAY_PROXY_URL" => Some("https://proxy.example:443".to_owned()),
                    _ => None,
                }
            )
            .unwrap_err(),
            CliError::UnsupportedRelayProxy(url) if url == "https://proxy.example:443"
        ));
    }

    #[test]
    fn rejects_invalid_listen_address_for_serve() {
        assert!(matches!(
            CliCommand::parse(["--listen".to_owned(), "127.0.0.1".to_owned()]).unwrap_err(),
            CliError::InvalidListenAddress(_)
        ));
        assert!(matches!(
            CliCommand::parse(["--listen".to_owned()]).unwrap_err(),
            CliError::MissingListenValue
        ));
    }

    #[test]
    fn rejects_incomplete_tls_config_for_serve() {
        assert!(matches!(
            CliCommand::parse([
                "--tls-cert".to_owned(),
                "/etc/termd/fullchain.pem".to_owned(),
            ])
            .unwrap_err(),
            CliError::IncompleteTlsConfig
        ));
    }

    #[test]
    fn parses_pair_with_default_url() {
        assert_eq!(
            CliCommand::parse(["pair".to_owned()]).unwrap(),
            CliCommand::Pair {
                url: DEFAULT_PAIRING_URL.to_owned(),
                qr: false,
                qr_svg: None,
            }
        );
    }

    #[test]
    fn parses_pair_with_custom_url() {
        assert_eq!(
            CliCommand::parse([
                "pair".to_owned(),
                "--url".to_owned(),
                "http://127.0.0.1:9999".to_owned(),
            ])
            .unwrap(),
            CliCommand::Pair {
                url: "http://127.0.0.1:9999".to_owned(),
                qr: false,
                qr_svg: None,
            }
        );
    }

    #[test]
    fn parses_pair_with_https_url() {
        assert_eq!(
            CliCommand::parse([
                "pair".to_owned(),
                "--url".to_owned(),
                "https://127.0.0.1:8765".to_owned(),
            ])
            .unwrap(),
            CliCommand::Pair {
                url: "https://127.0.0.1:8765".to_owned(),
                qr: false,
                qr_svg: None,
            }
        );
    }

    #[test]
    fn parses_pair_with_qr() {
        assert_eq!(
            CliCommand::parse(["pair".to_owned(), "--qr".to_owned(),]).unwrap(),
            CliCommand::Pair {
                url: DEFAULT_PAIRING_URL.to_owned(),
                qr: true,
                qr_svg: None,
            }
        );
    }

    #[test]
    fn parses_pair_with_qr_svg_path() {
        assert_eq!(
            CliCommand::parse([
                "pair".to_owned(),
                "--qr-svg".to_owned(),
                "/tmp/termd-pair.svg".to_owned(),
            ])
            .unwrap(),
            CliCommand::Pair {
                url: DEFAULT_PAIRING_URL.to_owned(),
                qr: false,
                qr_svg: Some(PathBuf::from("/tmp/termd-pair.svg")),
            }
        );
    }

    #[test]
    fn builds_pairing_qr_payload_from_http_response() {
        let response = PairingTokenResponse {
            token: PairingToken("pair-token".to_owned()),
            expires_at_ms: UnixTimestampMillis(1_710_000_060_000),
            server_id: ServerId::new(),
            daemon_public_key: PublicKey("ed25519-v1:daemon-public".to_owned()),
        };
        let payload = build_pairing_qr_payload(&response).unwrap();

        assert_eq!(payload.payload_type, PairingQrPayload::PAYLOAD_TYPE);
        assert_eq!(payload.version, PairingQrPayload::VERSION);
        assert_eq!(payload.token.0, "pair-token");
        assert_eq!(
            payload.daemon_public_key.as_ref().map(|key| key.0.as_str()),
            Some("ed25519-v1:daemon-public")
        );
        assert!(payload.ws_url.is_none());
        assert!(payload.is_supported_version());
    }

    #[test]
    fn relay_pairing_url_does_not_include_legacy_transport_token() {
        let relay_url = RelayBaseUrl::parse("wss://relay.example/ws").unwrap();

        // 中文注释：trusted relay 的浏览器入口靠 PairTicket admission，
        // pairing invite 不能再携带长期 relay transport token。
        assert_eq!(relay_url.client_url_template(), "wss://relay.example/ws");
    }

    #[test]
    fn rejects_unknown_argument_and_unsupported_url() {
        assert!(matches!(
            CliCommand::parse(["pair".to_owned(), "--bad".to_owned()]).unwrap_err(),
            CliError::UnexpectedArgument(argument) if argument == "--bad"
        ));
        assert!(matches!(
            CliCommand::parse([
                "--relay".to_owned(),
                "http://127.0.0.1:8080".to_owned(),
            ])
            .unwrap_err(),
            CliError::UnsupportedRelayUrl(url) if url == "http://127.0.0.1:8080"
        ));
    }

    #[test]
    fn help_text_uses_updated_trusted_relay_daemon_token_example() {
        assert!(HELP_TEXT.contains("/etc/termd/termd_daemon_token"));
        assert!(!HELP_TEXT.contains("/etc/termd/daemon_token"));
    }

    #[test]
    fn cli_error_display_redacts_secret_like_arguments_and_urls() {
        let cases = [
            CliError::UnexpectedArgument(
                "--relay=https://user:secret@example.invalid/ws?token=secret".to_owned(),
            ),
            CliError::UnsupportedUrl("https://user:secret@example.invalid?token=secret".to_owned()),
            CliError::UnsupportedRelayUrl(
                "wss://user:secret@relay.example/ws?token=secret".to_owned(),
            ),
            CliError::UnsupportedRelayProxy(
                "http://user:secret@proxy.example:3128/?token=secret".to_owned(),
            ),
            CliError::InvalidUrl("https://user:secret@example.invalid?token=secret".to_owned()),
        ];

        for error in cases {
            let rendered = error.to_string();
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("user:"));
            assert!(!rendered.contains("?token="));
        }
    }

    #[test]
    fn parses_pairing_token_http_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"token\":\"termd-pair-test\",\"expires_at_ms\":1710000060000,\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"ed25519-v1:daemon-public\"}";
        let parsed = parse_pairing_token_http_response(raw).unwrap();

        assert_eq!(parsed.token.0, "termd-pair-test");
        assert_eq!(parsed.expires_at_ms, UnixTimestampMillis(1_710_000_060_000));
        assert_eq!(
            parsed.server_id.0.to_string(),
            "00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(parsed.daemon_public_key.0, "ed25519-v1:daemon-public");
    }

    #[test]
    fn non_success_http_status_does_not_leak_body() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\r\n{\"token\":\"termd-pair-secret\",\"message\":\"secret detail\"}";
        let error = parse_pairing_token_http_response(raw).unwrap_err();
        let rendered = error.to_string();

        assert!(matches!(error, CliError::HttpStatus { status: 500 }));
        assert!(!rendered.contains("termd-pair-secret"));
        assert!(!rendered.contains("secret detail"));
    }
}
