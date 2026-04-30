use std::error::Error;
use std::fmt;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;
use termd::config::DaemonConfig;
use termd::net::relay::{RelayBaseUrl, connect_relay_mux};
use termd::net::server::{default_protocol, serve};

const DEFAULT_PAIRING_URL: &str = "http://127.0.0.1:8765";
const LOCAL_PAIRING_TOKEN_PATH: &str = "/local/pairing-token";
const LOCAL_PAIRING_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "termd=info,tower_http=warn".into()),
        )
        .init();

    match CliCommand::parse(std::env::args().skip(1))? {
        CliCommand::Serve { relay_url } => serve_daemon(relay_url).await?,
        CliCommand::Pair { url } => {
            let token = request_pairing_token(&url)?;
            println!("{token}");
        }
    }

    Ok(())
}

async fn serve_daemon(relay_url: Option<String>) -> Result<(), Box<dyn Error>> {
    // MVP 默认只监听 127.0.0.1:8765；更复杂的配置文件/后台守护留给后续任务。
    let config = DaemonConfig::default();
    let protocol = default_protocol(config.clone());

    tracing::info!(
        host = %config.listen_host,
        port = config.listen_port,
        "starting termd HTTP/WebSocket daemon"
    );

    if let Some(relay_url) = relay_url {
        let relay_protocol = protocol.clone();
        let relay_task = tokio::spawn(async move {
            connect_relay_mux(&relay_url, relay_protocol)
                .await
                .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })
        });

        tokio::select! {
            result = serve(config, protocol) => result?,
            relay_result = relay_task => {
                match relay_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(Box::new(error)),
                }
            }
        }
    } else {
        serve(config, protocol).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Serve { relay_url: Option<String> },
    Pair { url: String },
}

impl CliCommand {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter();
        let Some(command) = args.next() else {
            return Ok(Self::Serve { relay_url: None });
        };

        match command.as_str() {
            "pair" => parse_pair_args(args),
            "--relay" | "--relay-url" => {
                let relay_url = args.next().ok_or(CliError::MissingRelayUrlValue)?;
                if let Some(extra) = args.next() {
                    return Err(CliError::UnexpectedArgument(extra));
                }
                let _ = RelayBaseUrl::parse(&relay_url)
                    .map_err(|_| CliError::UnsupportedRelayUrl(relay_url.clone()))?;
                Ok(Self::Serve {
                    relay_url: Some(relay_url),
                })
            }
            other => Err(CliError::UnknownCommand(other.to_owned())),
        }
    }
}

fn parse_pair_args(mut args: impl Iterator<Item = String>) -> Result<CliCommand, CliError> {
    let mut url = DEFAULT_PAIRING_URL.to_owned();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => {
                url = args.next().ok_or(CliError::MissingUrlValue)?;
            }
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
    }

    // 解析阶段先拒绝不支持的 URL，避免用户等到网络请求时才看到模糊错误。
    let _ = parse_pairing_base_url(&url)?;
    Ok(CliCommand::Pair { url })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PairingBaseUrl {
    authority: String,
}

fn parse_pairing_base_url(url: &str) -> Result<PairingBaseUrl, CliError> {
    let Some(rest) = url.strip_prefix("http://") else {
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

    validate_host_port_authority(authority, url)?;
    Ok(PairingBaseUrl {
        authority: authority.to_owned(),
    })
}

fn validate_host_port_authority(authority: &str, original_url: &str) -> Result<(), CliError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((host, port)) = after_bracket.split_once("]:") else {
            return Err(CliError::InvalidUrl(original_url.to_owned()));
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(CliError::InvalidUrl(original_url.to_owned()));
        }
        return Ok(());
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Err(CliError::InvalidUrl(original_url.to_owned()));
    };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(CliError::InvalidUrl(original_url.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PairingTokenResponse {
    token: String,
}

fn request_pairing_token(base_url: &str) -> Result<String, CliError> {
    let endpoint = parse_pairing_base_url(base_url)?;
    let mut stream =
        TcpStream::connect(&endpoint.authority).map_err(|_| CliError::ConnectFailed)?;
    stream
        .set_read_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;
    stream
        .set_write_timeout(Some(LOCAL_PAIRING_HTTP_TIMEOUT))
        .map_err(|_| CliError::LocalIo)?;

    // 本地管理端点只签发 token，不携带请求体，避免把敏感材料写入日志或 shell history。
    let request = format!(
        "POST {LOCAL_PAIRING_TOKEN_PATH} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        endpoint.authority
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| CliError::SendFailed)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|_| CliError::ReceiveFailed)?;
    parse_pairing_token_http_response(&response)
}

fn parse_pairing_token_http_response(response: &[u8]) -> Result<String, CliError> {
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
    if payload.token.is_empty() {
        return Err(CliError::InvalidJson);
    }
    Ok(payload.token)
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
    MissingUrlValue,
    MissingRelayUrlValue,
    UnsupportedUrl(String),
    UnsupportedRelayUrl(String),
    InvalidUrl(String),
    ConnectFailed,
    SendFailed,
    ReceiveFailed,
    LocalIo,
    InvalidHttpResponse,
    HttpStatus { status: u16 },
    InvalidJson,
}

impl CliError {
    fn usage() -> &'static str {
        "usage: termd [--relay ws://127.0.0.1:8080] [pair [--url http://127.0.0.1:8765]]"
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
            Self::MissingUrlValue => write!(f, "`--url` requires a value\n{}", Self::usage()),
            Self::MissingRelayUrlValue => {
                write!(f, "`--relay` requires a value\n{}", Self::usage())
            }
            Self::UnsupportedUrl(url) => {
                write!(
                    f,
                    "unsupported daemon URL `{url}`; expected http://host:port"
                )
            }
            Self::UnsupportedRelayUrl(url) => {
                write!(f, "unsupported relay URL `{url}`; expected ws://host:port")
            }
            Self::InvalidUrl(url) => {
                write!(f, "invalid daemon URL `{url}`; expected http://host:port")
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
            CliCommand::Serve { relay_url: None }
        );
    }

    #[test]
    fn parses_relay_url_for_serve() {
        assert_eq!(
            CliCommand::parse(["--relay".to_owned(), "ws://127.0.0.1:8080/".to_owned()]).unwrap(),
            CliCommand::Serve {
                relay_url: Some("ws://127.0.0.1:8080/".to_owned())
            }
        );
        assert_eq!(
            CliCommand::parse(["--relay-url".to_owned(), "ws://127.0.0.1:8080".to_owned()])
                .unwrap(),
            CliCommand::Serve {
                relay_url: Some("ws://127.0.0.1:8080".to_owned())
            }
        );
    }

    #[test]
    fn parses_pair_with_default_url() {
        assert_eq!(
            CliCommand::parse(["pair".to_owned()]).unwrap(),
            CliCommand::Pair {
                url: DEFAULT_PAIRING_URL.to_owned()
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
                url: "http://127.0.0.1:9999".to_owned()
            }
        );
    }

    #[test]
    fn rejects_unknown_argument_and_unsupported_url() {
        assert!(matches!(
            CliCommand::parse(["pair".to_owned(), "--bad".to_owned()]).unwrap_err(),
            CliError::UnexpectedArgument(argument) if argument == "--bad"
        ));
        assert!(matches!(
            CliCommand::parse([
                "pair".to_owned(),
                "--url".to_owned(),
                "https://127.0.0.1:8765".to_owned(),
            ])
            .unwrap_err(),
            CliError::UnsupportedUrl(url) if url == "https://127.0.0.1:8765"
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
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"token\":\"termd-pair-test\"}";
        assert_eq!(
            parse_pairing_token_http_response(raw).unwrap(),
            "termd-pair-test"
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
