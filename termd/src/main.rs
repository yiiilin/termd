use std::error::Error;
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qrcode::{QrCode, render::unicode};
use serde::Deserialize;
use termd::config::{DaemonConfig, normalize_relay_endpoints};
use termd::net::relay::{RelayBaseUrl, RelayReconnectPolicy, run_relay_mux_with_reconnect};
use termd::net::server::{TlsPaths, serve, serve_tls, try_default_protocol};
use termd_proto::{PairingQrPayload, PairingToken, ServerId, UnixTimestampMillis};
use tokio::task::JoinHandle;

const DEFAULT_PAIRING_URL: &str = "http://127.0.0.1:8765";
const LOCAL_PAIRING_TOKEN_PATH: &str = "/local/pairing-token";
const LOCAL_PAIRING_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const HELP_TEXT: &str = concat!(
    "termd ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "USAGE:\n",
    "  termd [OPTIONS]\n",
    "  termd pair [OPTIONS]\n\n",
    "OPTIONS:\n",
    "  --listen <HOST:PORT>           Listen address, default 127.0.0.1:8765\n",
    "  --relay <WS_URL>               Connect to a relay; repeatable\n",
    "  --relay-url <WS_URL>           Alias for --relay\n",
    "  --relay-auth-token <TOKEN>     Transport auth token for relay connections\n",
    "  --tls-cert <CERT_PEM>          TLS certificate path\n",
    "  --tls-key <KEY_PEM>            TLS private key path; must be paired with --tls-cert\n",
    "  --web                          Serve embedded Web UI\n",
    "  -h, --help                     Print help\n",
    "  -V, --version                  Print version\n\n",
    "PAIR OPTIONS:\n",
    "  --url <HTTP_URL>               Local daemon URL, default http://127.0.0.1:8765\n",
    "  --qr                           Print a QR invite code for Web/mobile pairing\n",
    "  --ws-url <WS_URL>              WebSocket URL embedded in invite code; requires --qr\n\n",
    "EXAMPLES:\n",
    "  termd --listen 0.0.0.0:8765 --web\n",
    "  termd --relay wss://relay.example:443 --relay-auth-token env-token\n",
    "  termd pair --url http://127.0.0.1:8765\n",
    "  termd pair --qr --ws-url ws://192.168.1.20:8765/ws\n",
);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
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
            relay_auth_token,
            tls,
            web,
        } => serve_daemon(listen, relay_urls, relay_auth_token, tls, web).await?,
        CliCommand::Pair { url, qr, ws_url } => {
            let token = request_pairing_token_response(&url)?;
            if qr {
                let payload = build_pairing_qr_payload(&token, ws_url.as_deref())?;
                let invite_code = payload.to_invite_code();
                println!("{}", render_pairing_qr(&invite_code)?);
                println!("{invite_code}");
            } else {
                println!("{}", token.token.0);
            }
        }
    }

    Ok(())
}

async fn serve_daemon(
    listen: Option<ListenAddress>,
    relay_urls: Vec<String>,
    relay_auth_token: Option<String>,
    tls: Option<TlsPaths>,
    web_enabled: bool,
) -> Result<(), Box<dyn Error>> {
    // MVP 默认只监听 127.0.0.1:8765；更复杂的配置文件/后台守护留给后续任务。
    let mut config = DaemonConfig::default();
    if let Some(listen) = listen {
        config.listen_host = listen.host;
        config.listen_port = listen.port;
    }
    let protocol = try_default_protocol(config.clone())?;
    let relay_endpoints = normalize_relay_endpoints(
        config
            .relay_endpoints
            .clone()
            .into_iter()
            .chain(relay_urls.into_iter()),
    )?;

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
            relay_auth_token,
            reconnect_policy,
            relay_protocol,
        );
    }
    serve_with_optional_tls(config, protocol, tls, web_enabled).await?;
    Ok(())
}

fn spawn_relay_reconnect_supervisors(
    relay_endpoints: Vec<String>,
    relay_auth_token: Option<String>,
    reconnect_policy: RelayReconnectPolicy,
    protocol: termd::net::server::SharedDaemonProtocol,
) -> Vec<JoinHandle<()>> {
    relay_endpoints
        .into_iter()
        .map(|relay_url| {
            let relay_protocol = protocol.clone();
            let relay_auth_token = relay_auth_token.clone();
            tokio::spawn(async move {
                // 每个 relay endpoint 都有自己独立的 supervisor，避免一个端点的失败拖住其他端点。
                if let Err(error) = run_relay_mux_with_reconnect(
                    &relay_url,
                    relay_auth_token.as_deref(),
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
        relay_auth_token: Option<String>,
        tls: Option<TlsPaths>,
        web: bool,
    },
    Pair {
        url: String,
        qr: bool,
        ws_url: Option<String>,
    },
}

impl fmt::Debug for CliCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Help => formatter.write_str("Help"),
            Self::Version => formatter.write_str("Version"),
            Self::Serve {
                listen,
                relay_urls,
                relay_auth_token,
                tls,
                web,
            } => {
                // relay auth token 是 transport 凭证，命令解析测试需要保留真实值用于行为断言，
                // 但 Debug 输出只能暴露是否配置，避免后续错误日志中误带明文 token。
                formatter
                    .debug_struct("Serve")
                    .field("listen", listen)
                    .field("relay_urls", relay_urls)
                    .field("relay_auth_token_configured", &relay_auth_token.is_some())
                    .field("tls", tls)
                    .field("web", web)
                    .finish()
            }
            Self::Pair { url, qr, ws_url } => formatter
                .debug_struct("Pair")
                .field("url", url)
                .field("qr", qr)
                .field("ws_url", ws_url)
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
                relay_auth_token: None,
                tls: None,
                web: false,
            });
        };

        match command.as_str() {
            "-h" | "--help" | "help" => Ok(Self::Help),
            "-V" | "--version" | "version" => Ok(Self::Version),
            "pair" => parse_pair_args(args),
            "--listen" | "--relay" | "--relay-url" | "--relay-auth-token" | "--tls-cert"
            | "--tls-key" | "--web" => parse_serve_args(std::iter::once(command).chain(args)),
            other => Err(CliError::UnknownCommand(other.to_owned())),
        }
    }
}

fn parse_serve_args(args: impl IntoIterator<Item = String>) -> Result<CliCommand, CliError> {
    let mut listen = None;
    let mut relay_urls = Vec::new();
    let mut relay_auth_token = None;
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
                relay_urls.push(value);
            }
            "--relay-auth-token" => {
                let value = args.next().ok_or(CliError::MissingRelayAuthTokenValue)?;
                if value.is_empty() {
                    return Err(CliError::EmptyRelayAuthTokenValue);
                }
                relay_auth_token = Some(value);
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
        relay_auth_token,
        tls,
        web,
    })
}

fn parse_pair_args(mut args: impl Iterator<Item = String>) -> Result<CliCommand, CliError> {
    let mut url = DEFAULT_PAIRING_URL.to_owned();
    let mut qr = false;
    let mut ws_url = None;

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
            "--ws-url" => {
                if !qr {
                    return Err(CliError::WsUrlRequiresQr);
                }
                let value = args.next().ok_or(CliError::MissingWsUrlValue)?;
                if value.trim().is_empty() {
                    return Err(CliError::EmptyWsUrlValue);
                }
                ws_url = Some(value);
            }
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
    }

    // 解析阶段先拒绝不支持的 URL，避免用户等到网络请求时才看到模糊错误。
    let _ = parse_pairing_base_url(&url)?;
    Ok(CliCommand::Pair { url, qr, ws_url })
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

fn build_pairing_qr_payload(
    token: &PairingTokenResponse,
    ws_url: Option<&str>,
) -> Result<PairingQrPayload, CliError> {
    let default_config = DaemonConfig::default();
    let ws_url_template = ws_url.unwrap_or(&default_config.default_pairing_ws_url);
    let ws_url = resolve_pairing_ws_url(ws_url_template, token.server_id)?;

    Ok(PairingQrPayload::new(
        ws_url,
        token.token.clone(),
        token.server_id,
        token.expires_at_ms,
    ))
}

fn resolve_pairing_ws_url(template: &str, server_id: ServerId) -> Result<String, CliError> {
    let rendered = template
        .trim()
        .replace("{server_id}", &server_id.0.to_string());
    if rendered.is_empty() {
        return Err(CliError::EmptyWsUrlValue);
    }
    if !is_supported_ws_url(&rendered) {
        return Err(CliError::InvalidWsUrl(rendered));
    }
    Ok(rendered)
}

fn is_supported_ws_url(value: &str) -> bool {
    (value.starts_with("ws://") || value.starts_with("wss://"))
        && !value.chars().any(char::is_whitespace)
        && !value.contains('#')
}

fn render_pairing_qr(invite_code: &str) -> Result<String, CliError> {
    let code = QrCode::new(invite_code.as_bytes()).map_err(|_| CliError::QrRenderFailed)?;

    // 终端输出用 Unicode 二维码；邀请码会在下一行单独打印，便于复制粘贴。
    Ok(code.render::<unicode::Dense1x2>().build())
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
    MissingRelayUrlValue,
    MissingRelayAuthTokenValue,
    MissingTlsCertValue,
    MissingTlsKeyValue,
    MissingWsUrlValue,
    EmptyRelayAuthTokenValue,
    EmptyTlsCertValue,
    EmptyTlsKeyValue,
    EmptyWsUrlValue,
    IncompleteTlsConfig,
    WsUrlRequiresQr,
    UnsupportedUrl(String),
    UnsupportedRelayUrl(String),
    InvalidListenAddress(String),
    InvalidUrl(String),
    InvalidWsUrl(String),
    ConnectFailed,
    SendFailed,
    ReceiveFailed,
    LocalIo,
    InvalidHttpResponse,
    HttpStatus { status: u16 },
    InvalidJson,
    QrRenderFailed,
    Tls,
}

impl CliError {
    fn usage() -> &'static str {
        "usage: termd [--listen 127.0.0.1:8765] [--relay ws://host:port]... [--relay-auth-token <token>] [--tls-cert <cert.pem> --tls-key <key.pem>] [--web] [pair [--url http://127.0.0.1:8765|https://127.0.0.1:8765] [--qr [--ws-url ws://127.0.0.1:8765/ws]]]\ntry `termd --help` for full help"
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(command) => {
                write!(f, "unknown termd command `{command}`\n{}", Self::usage())
            }
            Self::UnexpectedArgument(argument) => {
                write!(f, "unexpected argument `{argument}`\n{}", Self::usage())
            }
            Self::MissingListenValue => write!(f, "`--listen` requires a value\n{}", Self::usage()),
            Self::MissingUrlValue => write!(f, "`--url` requires a value\n{}", Self::usage()),
            Self::MissingRelayUrlValue => {
                write!(f, "`--relay` requires a value\n{}", Self::usage())
            }
            Self::MissingRelayAuthTokenValue => {
                write!(
                    f,
                    "`--relay-auth-token` requires a value\n{}",
                    Self::usage()
                )
            }
            Self::MissingTlsCertValue => {
                write!(f, "`--tls-cert` requires a value\n{}", Self::usage())
            }
            Self::MissingTlsKeyValue => {
                write!(f, "`--tls-key` requires a value\n{}", Self::usage())
            }
            Self::MissingWsUrlValue => write!(f, "`--ws-url` requires a value\n{}", Self::usage()),
            Self::EmptyRelayAuthTokenValue => {
                write!(
                    f,
                    "`--relay-auth-token` requires a non-empty value\n{}",
                    Self::usage()
                )
            }
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
            Self::EmptyWsUrlValue => {
                write!(
                    f,
                    "`--ws-url` requires a non-empty value\n{}",
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
            Self::WsUrlRequiresQr => {
                write!(
                    f,
                    "`--ws-url` can only be used with `--qr`\n{}",
                    Self::usage()
                )
            }
            Self::UnsupportedUrl(url) => write!(
                f,
                "unsupported daemon URL `{url}`; expected http://host:port or https://host:port"
            ),
            Self::UnsupportedRelayUrl(url) => {
                write!(
                    f,
                    "unsupported relay URL `{url}`; expected ws://host:port or wss://host:port"
                )
            }
            Self::InvalidListenAddress(address) => {
                write!(
                    f,
                    "invalid listen address `{address}`; expected host:port such as 127.0.0.1:8765"
                )
            }
            Self::InvalidUrl(url) => {
                write!(
                    f,
                    "invalid daemon URL `{url}`; expected http://host:port or https://host:port"
                )
            }
            Self::InvalidWsUrl(url) => {
                write!(
                    f,
                    "invalid pairing WebSocket URL `{url}`; expected ws://... or wss://..."
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
            Self::Tls => write!(f, "failed to establish TLS for pairing token request"),
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
                relay_auth_token: None,
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
                relay_auth_token: None,
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
                relay_auth_token: None,
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
                relay_auth_token: None,
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
                relay_auth_token: None,
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
                relay_auth_token: None,
                tls: None,
                web: false,
            }
        );
    }

    #[test]
    fn parses_multiple_relay_urls_for_serve() {
        let command = CliCommand::parse([
            "--relay".to_owned(),
            "ws://127.0.0.1:8080".to_owned(),
            "--relay-url".to_owned(),
            "wss://relay.example:443".to_owned(),
        ])
        .unwrap();

        let rendered = format!("{command:?}");
        assert!(rendered.contains("ws://127.0.0.1:8080"));
        assert!(rendered.contains("wss://relay.example:443"));
    }

    #[test]
    fn parses_web_flag_for_serve() {
        assert_eq!(
            CliCommand::parse(["--web".to_owned()]).unwrap(),
            CliCommand::Serve {
                listen: None,
                relay_urls: Vec::new(),
                relay_auth_token: None,
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
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use termd::config::RelayReconnectConfig;

        #[derive(Clone, Default)]
        struct MockMuxState {
            attempts: Arc<AtomicUsize>,
            heartbeat_pings: Arc<AtomicUsize>,
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
                    return;
                }

                while let Some(message) = socket.next().await {
                    match message {
                        Ok(AxumMessage::Ping(payload)) => {
                            state.heartbeat_pings.fetch_add(1, Ordering::SeqCst);
                            let _ = socket.send(AxumMessage::Pong(payload)).await;
                            break;
                        }
                        Ok(AxumMessage::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            })
        }

        let flaky_state = MockMuxState {
            close_first_attempt: true,
            ..MockMuxState::default()
        };
        let healthy_state = MockMuxState::default();

        let flaky_app = axum::Router::new()
            .route("/ws/:server_id/daemon-mux", get(mock_daemon_mux_ws))
            .with_state(flaky_state.clone());
        let healthy_app = axum::Router::new()
            .route("/ws/:server_id/daemon-mux", get(mock_daemon_mux_ws))
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
        let protocol = termd::net::server::default_protocol(DaemonConfig::default_for_state_path(
            std::env::temp_dir().join(format!(
                "termd-main-test-{}-relay-state.json",
                std::process::id()
            )),
        ));
        let relay_tasks = spawn_relay_reconnect_supervisors(
            vec![format!("ws://{flaky_addr}"), format!("ws://{healthy_addr}")],
            None,
            reconnect_policy,
            protocol,
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if flaky_state.attempts.load(Ordering::SeqCst) >= 2
                    && healthy_state.heartbeat_pings.load(Ordering::SeqCst) >= 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        for handle in relay_tasks {
            handle.abort();
        }
        flaky_server.abort();
        healthy_server.abort();
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
                relay_auth_token: None,
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
    fn parses_relay_auth_token_for_serve_without_debug_leakage() {
        let command = CliCommand::parse([
            "--relay".to_owned(),
            "ws://127.0.0.1:8080".to_owned(),
            "--relay-auth-token".to_owned(),
            "relay-secret-1".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Serve {
                listen: None,
                relay_urls: vec!["ws://127.0.0.1:8080".to_owned()],
                relay_auth_token: Some("relay-secret-1".to_owned()),
                tls: None,
                web: false,
            }
        );
        assert!(!format!("{command:?}").contains("relay-secret-1"));
    }

    #[test]
    fn rejects_empty_relay_auth_token_for_serve() {
        assert!(matches!(
            CliCommand::parse([
                "--relay".to_owned(),
                "ws://127.0.0.1:8080".to_owned(),
                "--relay-auth-token".to_owned(),
                String::new(),
            ])
            .unwrap_err(),
            CliError::EmptyRelayAuthTokenValue
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
                ws_url: None,
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
                ws_url: None,
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
                ws_url: None,
            }
        );
    }

    #[test]
    fn parses_pair_with_qr_and_custom_ws_url() {
        assert_eq!(
            CliCommand::parse([
                "pair".to_owned(),
                "--qr".to_owned(),
                "--ws-url".to_owned(),
                "wss://relay.example/ws/{server_id}/client".to_owned(),
            ])
            .unwrap(),
            CliCommand::Pair {
                url: DEFAULT_PAIRING_URL.to_owned(),
                qr: true,
                ws_url: Some("wss://relay.example/ws/{server_id}/client".to_owned()),
            }
        );
    }

    #[test]
    fn rejects_ws_url_without_qr() {
        assert!(matches!(
            CliCommand::parse([
                "pair".to_owned(),
                "--ws-url".to_owned(),
                "ws://127.0.0.1:8765/ws".to_owned(),
            ])
            .unwrap_err(),
            CliError::WsUrlRequiresQr
        ));
    }

    #[test]
    fn builds_pairing_qr_payload_from_http_response() {
        let response = PairingTokenResponse {
            token: PairingToken("pair-token".to_owned()),
            expires_at_ms: UnixTimestampMillis(1_710_000_060_000),
            server_id: ServerId::new(),
        };
        let payload =
            build_pairing_qr_payload(&response, Some("wss://relay.example/ws/{server_id}/client"))
                .unwrap();

        assert_eq!(payload.payload_type, PairingQrPayload::PAYLOAD_TYPE);
        assert_eq!(payload.version, PairingQrPayload::VERSION);
        assert_eq!(payload.token.0, "pair-token");
        assert!(payload.ws_url.contains(&response.server_id.0.to_string()));
        assert!(payload.is_supported_version());
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
    fn parses_pairing_token_http_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"token\":\"termd-pair-test\",\"expires_at_ms\":1710000060000,\"server_id\":\"00000000-0000-0000-0000-000000000001\"}";
        let parsed = parse_pairing_token_http_response(raw).unwrap();

        assert_eq!(parsed.token.0, "termd-pair-test");
        assert_eq!(parsed.expires_at_ms, UnixTimestampMillis(1_710_000_060_000));
        assert_eq!(
            parsed.server_id.0.to_string(),
            "00000000-0000-0000-0000-000000000001"
        );
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
