use std::env;
use std::ffi::OsString;
use std::fmt;
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;

use thiserror::Error;

/// 公网部署时通常只绑定内网或 loopback，再由反向代理对外提供 WSS。
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

#[derive(Clone, PartialEq, Eq)]
pub struct Args {
    /// relay 的内部监听地址；公网入口通常交给反向代理，不直接暴露这里的端口。
    pub listen: SocketAddr,
    /// relay transport 凭证；部署时通常由 secret manager 注入，不应进入日志。
    pub auth_token: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// 是否挂载内嵌 Web 静态资源；默认关闭，避免 relay 默认暴露 UI 面。
    pub web: bool,
}

impl fmt::Debug for Args {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // relay auth token 是 transport 凭证，Debug 输出只能显示是否配置，不能泄漏值。
        formatter
            .debug_struct("Args")
            .field("listen", &self.listen)
            .field("auth_token_configured", &self.auth_token.is_some())
            .field("tls_cert", &self.tls_cert)
            .field("tls_key_configured", &self.tls_key.is_some())
            .field("web", &self.web)
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArgsError {
    #[error("unknown argument: {0}")]
    UnknownArgument(String),
    #[error("{0} requires a value")]
    MissingValue(&'static str),
    #[error("{0} requires a non-empty value")]
    EmptyValue(&'static str),
    #[error("invalid listen address")]
    InvalidListenAddress(#[from] AddrParseError),
    #[error("TLS cert and key must be configured together")]
    IncompleteTlsConfig,
}

impl Args {
    pub fn from_env() -> Result<Self, ArgsError> {
        Self::parse_from(env::args_os())
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut listen = DEFAULT_LISTEN.parse()?;
        let mut auth_token = None;
        let mut tls_cert = None;
        let mut tls_key = None;
        let mut web = false;
        let mut args = args.into_iter().map(Into::into);

        // 第一个参数是程序名；不要求存在，方便单元测试直接传空迭代器。
        let _program = args.next();

        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy().into_owned();
            match arg.as_str() {
                "--listen" | "-l" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--listen"))?;
                    listen = value.to_string_lossy().parse()?;
                }
                "--auth-token" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--auth-token"))?;
                    let value = value.to_string_lossy().into_owned();
                    if value.is_empty() {
                        return Err(ArgsError::EmptyValue("--auth-token"));
                    }
                    auth_token = Some(value);
                }
                "--tls-cert" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--tls-cert"))?;
                    let value = value.to_string_lossy().into_owned();
                    if value.is_empty() {
                        return Err(ArgsError::EmptyValue("--tls-cert"));
                    }
                    tls_cert = Some(PathBuf::from(value));
                }
                "--tls-key" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--tls-key"))?;
                    let value = value.to_string_lossy().into_owned();
                    if value.is_empty() {
                        return Err(ArgsError::EmptyValue("--tls-key"));
                    }
                    tls_key = Some(PathBuf::from(value));
                }
                "--web" => {
                    web = true;
                }
                other => return Err(ArgsError::UnknownArgument(other.to_owned())),
            }
        }

        if tls_cert.is_some() != tls_key.is_some() {
            return Err(ArgsError::IncompleteTlsConfig);
        }

        Ok(Self {
            listen,
            auth_token,
            tls_cert,
            tls_key,
            web,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_localhost_default_listen_address() {
        let args = Args::parse_from(["termrelay"]).unwrap();

        assert_eq!(args.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(args.auth_token, None);
        assert_eq!(args.tls_cert, None);
        assert_eq!(args.tls_key, None);
        assert!(!args.web);
    }

    #[test]
    fn parses_listen_argument() {
        let args = Args::parse_from(["termrelay", "--listen", "127.0.0.1:9000"]).unwrap();

        assert_eq!(args.listen, "127.0.0.1:9000".parse().unwrap());
    }

    #[test]
    fn parses_auth_token_without_debug_leakage() {
        let args = Args::parse_from(["termrelay", "--auth-token", "relay-secret-1"]).unwrap();

        assert_eq!(args.auth_token.as_deref(), Some("relay-secret-1"));
        assert!(!format!("{args:?}").contains("relay-secret-1"));
    }

    #[test]
    fn parses_tls_cert_and_key_without_debug_key_path_leakage() {
        let args = Args::parse_from([
            "termrelay",
            "--tls-cert",
            "/etc/termd/fullchain.pem",
            "--tls-key",
            "/etc/termd/secret-key.pem",
        ])
        .unwrap();

        assert_eq!(
            args.tls_cert,
            Some(PathBuf::from("/etc/termd/fullchain.pem"))
        );
        assert_eq!(
            args.tls_key,
            Some(PathBuf::from("/etc/termd/secret-key.pem"))
        );
        assert!(!format!("{args:?}").contains("secret-key.pem"));
    }

    #[test]
    fn parses_web_flag() {
        let args = Args::parse_from(["termrelay", "--web"]).unwrap();

        assert!(args.web);
    }

    #[test]
    fn rejects_missing_listen_value() {
        let error = Args::parse_from(["termrelay", "--listen"]).unwrap_err();

        assert_eq!(error, ArgsError::MissingValue("--listen"));
    }

    #[test]
    fn rejects_missing_auth_token_value() {
        let error = Args::parse_from(["termrelay", "--auth-token"]).unwrap_err();

        assert_eq!(error, ArgsError::MissingValue("--auth-token"));
    }

    #[test]
    fn rejects_empty_auth_token_value() {
        let error = Args::parse_from(["termrelay", "--auth-token", ""]).unwrap_err();

        assert_eq!(error, ArgsError::EmptyValue("--auth-token"));
    }

    #[test]
    fn rejects_incomplete_tls_config() {
        let error =
            Args::parse_from(["termrelay", "--tls-cert", "/etc/termd/fullchain.pem"]).unwrap_err();

        assert_eq!(error, ArgsError::IncompleteTlsConfig);
    }

    #[test]
    fn rejects_unknown_argument() {
        let error = Args::parse_from(["termrelay", "--auth"]).unwrap_err();

        assert_eq!(error, ArgsError::UnknownArgument("--auth".to_owned()));
    }
}
