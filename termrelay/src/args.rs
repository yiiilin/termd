use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;

use serde::Deserialize;
use termd_proto::ServerId;
use thiserror::Error;

/// 公网部署时通常只绑定内网或 loopback，再由反向代理对外提供 WSS。
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

#[derive(Clone, PartialEq, Eq)]
pub struct Args {
    /// relay 的内部监听地址；公网入口通常交给反向代理，不直接暴露这里的端口。
    pub listen: SocketAddr,
    /// relay transport 凭证；部署时通常由 secret manager 注入，不应进入日志。
    pub auth_token: Option<String>,
    /// daemon 自注册用的长期 setup token；只在安装/注册时由管理员使用。
    pub setup_token: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// trusted relay 的 daemon 注册表；生产入口缺失时默认拒绝启动。
    pub daemon_registry: DaemonRegistryConfig,
    /// daemon 注册表路径；注册 API 成功后会原子写回这个文件。
    pub daemon_registry_path: Option<PathBuf>,
    /// 显式允许旧开放 relay 行为，仅用于本地测试或迁移窗口。
    pub allow_open_relay: bool,
    /// 是否挂载内嵌 Web 静态资源；默认关闭，避免 relay 默认暴露 UI 面。
    pub web: bool,
    /// 是否启用 HTTP 文件 tunnel 兼容路径；默认关闭，避免 relay 默认暴露额外 HTTP 面。
    pub http_tunnel: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DaemonRegistryConfig {
    #[serde(default)]
    pub daemons: Vec<DaemonRegistryEntry>,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DaemonRegistryEntry {
    pub server_id: ServerId,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub token_hash: String,
}

impl DaemonRegistryEntry {
    pub fn runtime_credential(&self) -> Option<DaemonRegistryRuntimeCredential<'_>> {
        if !self.token_hash.is_empty() {
            Some(DaemonRegistryRuntimeCredential::TokenHash(
                self.token_hash.as_str(),
            ))
        } else if !self.token.is_empty() {
            Some(DaemonRegistryRuntimeCredential::PlainToken(
                self.token.as_str(),
            ))
        } else {
            None
        }
    }
}

pub enum DaemonRegistryRuntimeCredential<'a> {
    PlainToken(&'a str),
    TokenHash(&'a str),
}

impl fmt::Debug for DaemonRegistryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonRegistryEntry")
            .field("server_id", &self.server_id)
            .field("token_configured", &self.runtime_credential().is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayCommand {
    Serve(Args),
    Help,
    Version,
}

enum AuthTokenSource {
    Argument(String),
    File(PathBuf),
}

impl fmt::Debug for Args {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // relay auth token 是 transport 凭证，Debug 输出只能显示是否配置，不能泄漏值。
        formatter
            .debug_struct("Args")
            .field("listen", &self.listen)
            .field("auth_token_configured", &self.auth_token.is_some())
            .field("setup_token_configured", &self.setup_token.is_some())
            .field("tls_cert", &self.tls_cert)
            .field("tls_key_configured", &self.tls_key.is_some())
            .field("daemon_registry_count", &self.daemon_registry.daemons.len())
            .field("allow_open_relay", &self.allow_open_relay)
            .field("web", &self.web)
            .field("http_tunnel", &self.http_tunnel)
            .finish()
    }
}

impl RelayCommand {
    pub fn from_env() -> Result<Self, ArgsError> {
        Self::parse_from(env::args_os())
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        if args
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.to_string_lossy().as_ref(), "-h" | "--help" | "help"))
        {
            return Ok(Self::Help);
        }
        if args.iter().skip(1).any(|arg| {
            matches!(
                arg.to_string_lossy().as_ref(),
                "-V" | "--version" | "version"
            )
        }) {
            return Ok(Self::Version);
        }

        Args::parse_from(args).map(Self::Serve)
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
    #[error("failed to read auth token file")]
    ReadAuthTokenFile,
    #[error("failed to read setup token file")]
    ReadSetupTokenFile,
    #[error("failed to read daemon registry file")]
    ReadDaemonRegistryFile,
    #[error("failed to parse daemon registry file")]
    ParseDaemonRegistryFile,
    #[error("--auth-token and --auth-token-file cannot be used together")]
    ConflictingAuthTokenSources,
    #[error("invalid listen address")]
    InvalidListenAddress(#[from] AddrParseError),
    #[error("TLS cert and key must be configured together")]
    IncompleteTlsConfig,
}

impl Args {
    pub fn parse_from<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut listen = DEFAULT_LISTEN.parse()?;
        let mut auth_token_source = None;
        let mut setup_token_file = None;
        let mut tls_cert = None;
        let mut tls_key = None;
        let mut daemon_registry_file = None;
        let mut allow_open_relay = false;
        let mut web = false;
        let mut http_tunnel = false;
        let mut args = args.into_iter().map(Into::into);

        // 第一个参数是程序名；不要求存在，方便单元测试直接传空迭代器。
        let _program = args.next();

        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy().into_owned();
            if let Some(value) = arg.strip_prefix("--auth-token=") {
                set_auth_token_argument(&mut auth_token_source, value.to_owned())?;
                continue;
            }
            if let Some(value) = arg.strip_prefix("--auth-token-file=") {
                set_auth_token_file(&mut auth_token_source, value.to_owned())?;
                continue;
            }
            if let Some(value) = arg.strip_prefix("--setup-token-file=") {
                setup_token_file = Some(non_empty_path("--setup-token-file", value)?);
                continue;
            }
            if let Some(value) = arg.strip_prefix("--daemon-registry=") {
                daemon_registry_file = Some(non_empty_path("--daemon-registry", value)?);
                continue;
            }
            match arg.as_str() {
                "--listen" | "-l" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--listen"))?;
                    listen = value.to_string_lossy().parse()?;
                }
                "--auth-token" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--auth-token"))?;
                    set_auth_token_argument(
                        &mut auth_token_source,
                        value.to_string_lossy().into_owned(),
                    )?;
                }
                "--auth-token-file" => {
                    let value = args
                        .next()
                        .ok_or(ArgsError::MissingValue("--auth-token-file"))?;
                    set_auth_token_file(
                        &mut auth_token_source,
                        value.to_string_lossy().into_owned(),
                    )?;
                }
                "--setup-token-file" => {
                    let value = args
                        .next()
                        .ok_or(ArgsError::MissingValue("--setup-token-file"))?;
                    setup_token_file = Some(non_empty_path(
                        "--setup-token-file",
                        value.to_string_lossy().as_ref(),
                    )?);
                }
                "--daemon-registry" => {
                    let value = args
                        .next()
                        .ok_or(ArgsError::MissingValue("--daemon-registry"))?;
                    daemon_registry_file = Some(non_empty_path(
                        "--daemon-registry",
                        value.to_string_lossy().as_ref(),
                    )?);
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
                "--allow-open-relay" => {
                    allow_open_relay = true;
                }
                "--http-tunnel" => {
                    http_tunnel = true;
                }
                other => return Err(ArgsError::UnknownArgument(other.to_owned())),
            }
        }

        if tls_cert.is_some() != tls_key.is_some() {
            return Err(ArgsError::IncompleteTlsConfig);
        }

        let auth_token = resolve_auth_token_source(auth_token_source)?;
        let setup_token = match setup_token_file {
            Some(path) => read_setup_token_file(path).map(Some)?,
            None => None,
        };
        let daemon_registry_path = daemon_registry_file.clone();
        let daemon_registry = match daemon_registry_file {
            Some(path) => read_daemon_registry_file(path)?,
            None => DaemonRegistryConfig::default(),
        };

        Ok(Self {
            listen,
            auth_token,
            setup_token,
            tls_cert,
            tls_key,
            daemon_registry,
            daemon_registry_path,
            allow_open_relay,
            web,
            http_tunnel,
        })
    }
}

fn non_empty_path(flag: &'static str, value: &str) -> Result<PathBuf, ArgsError> {
    if value.is_empty() {
        return Err(ArgsError::EmptyValue(flag));
    }
    Ok(PathBuf::from(value))
}

fn set_auth_token_source(
    current: &mut Option<AuthTokenSource>,
    next: AuthTokenSource,
) -> Result<(), ArgsError> {
    if matches!(
        (current.as_ref(), &next),
        (Some(AuthTokenSource::Argument(_)), AuthTokenSource::File(_))
            | (Some(AuthTokenSource::File(_)), AuthTokenSource::Argument(_))
    ) {
        return Err(ArgsError::ConflictingAuthTokenSources);
    }

    *current = Some(next);
    Ok(())
}

fn set_auth_token_argument(
    current: &mut Option<AuthTokenSource>,
    value: String,
) -> Result<(), ArgsError> {
    if value.is_empty() {
        return Err(ArgsError::EmptyValue("--auth-token"));
    }
    set_auth_token_source(current, AuthTokenSource::Argument(value))
}

fn set_auth_token_file(
    current: &mut Option<AuthTokenSource>,
    value: String,
) -> Result<(), ArgsError> {
    if value.is_empty() {
        return Err(ArgsError::EmptyValue("--auth-token-file"));
    }
    set_auth_token_source(current, AuthTokenSource::File(PathBuf::from(value)))
}

fn resolve_auth_token_source(source: Option<AuthTokenSource>) -> Result<Option<String>, ArgsError> {
    match source {
        Some(AuthTokenSource::Argument(token)) => Ok(Some(token)),
        Some(AuthTokenSource::File(path)) => read_auth_token_file(path).map(Some),
        None => Ok(None),
    }
}

fn read_auth_token_file(path: PathBuf) -> Result<String, ArgsError> {
    let token = fs::read_to_string(path).map_err(|_| ArgsError::ReadAuthTokenFile)?;
    let token = trim_trailing_line_endings(token);
    if token.trim().is_empty() {
        return Err(ArgsError::EmptyValue("--auth-token-file"));
    }
    Ok(token)
}

fn read_setup_token_file(path: PathBuf) -> Result<String, ArgsError> {
    let token = fs::read_to_string(path).map_err(|_| ArgsError::ReadSetupTokenFile)?;
    let token = trim_trailing_line_endings(token);
    if token.trim().is_empty() {
        return Err(ArgsError::EmptyValue("--setup-token-file"));
    }
    Ok(token)
}

fn read_daemon_registry_file(path: PathBuf) -> Result<DaemonRegistryConfig, ArgsError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 中文注释：自动注册模式下 registry 是 relay 自己维护的状态文件；
            // 首次启动时文件可以不存在，注册 API 会在成功注册后原子创建。
            return Ok(DaemonRegistryConfig::default());
        }
        Err(_) => return Err(ArgsError::ReadDaemonRegistryFile),
    };
    serde_json::from_str(&raw).map_err(|_| ArgsError::ParseDaemonRegistryFile)
}

fn trim_trailing_line_endings(mut value: String) -> String {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn uses_localhost_default_listen_address() {
        let args = Args::parse_from(["termrelay"]).unwrap();

        assert_eq!(args.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(args.auth_token, None);
        assert_eq!(args.tls_cert, None);
        assert_eq!(args.tls_key, None);
        assert!(!args.web);
        assert!(!args.http_tunnel);
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
    fn parses_equals_auth_token_without_debug_leakage() {
        let args = Args::parse_from(["termrelay", "--auth-token=relay-secret-equals"]).unwrap();

        assert_eq!(args.auth_token.as_deref(), Some("relay-secret-equals"));
        assert!(!format!("{args:?}").contains("relay-secret-equals"));
    }

    #[test]
    fn parses_auth_token_file_without_debug_path_or_token_leakage() {
        let token_file = write_temp_auth_token("relay-secret-from-file\n\n");
        let token_file_arg = token_file.to_string_lossy().into_owned();

        let args = Args::parse_from(["termrelay", "--auth-token-file", &token_file_arg]).unwrap();

        assert_eq!(args.auth_token.as_deref(), Some("relay-secret-from-file"));
        let rendered = format!("{args:?}");
        assert!(!rendered.contains("relay-secret-from-file"));
        assert!(!rendered.contains(&token_file_arg));
        let _ = fs::remove_file(token_file);
    }

    #[test]
    fn parses_equals_auth_token_file_without_debug_path_or_token_leakage() {
        let token_file = write_temp_auth_token("relay-secret-from-equals-file\n");
        let token_file_arg = token_file.to_string_lossy().into_owned();
        let argument = format!("--auth-token-file={token_file_arg}");

        let args = Args::parse_from(["termrelay", &argument]).unwrap();

        assert_eq!(
            args.auth_token.as_deref(),
            Some("relay-secret-from-equals-file")
        );
        let rendered = format!("{args:?}");
        assert!(!rendered.contains("relay-secret-from-equals-file"));
        assert!(!rendered.contains(&token_file_arg));
        let _ = fs::remove_file(token_file);
    }

    #[test]
    fn parses_daemon_registry_file_without_debug_secret_leakage() {
        let registry_file = write_temp_registry(
            r#"{"daemons":[{"server_id":"00000000-0000-0000-0000-000000000123","token":"daemon-secret-1"}]}"#,
        );
        let registry_arg = registry_file.to_string_lossy().into_owned();

        let args = Args::parse_from(["termrelay", "--daemon-registry", &registry_arg]).unwrap();

        assert_eq!(args.daemon_registry.daemons.len(), 1);
        assert_eq!(
            args.daemon_registry.daemons[0].server_id.0.to_string(),
            "00000000-0000-0000-0000-000000000123"
        );
        assert_eq!(args.daemon_registry.daemons[0].token, "daemon-secret-1");
        let rendered = format!("{args:?}");
        assert!(rendered.contains("daemon_registry_count"));
        assert!(!rendered.contains("daemon-secret-1"));
        assert!(!rendered.contains(&registry_arg));
        let _ = fs::remove_file(registry_file);
    }

    #[test]
    fn parses_setup_token_file_without_debug_secret_leakage() {
        let token_file = write_temp_auth_token("relay-setup-secret-1\n");
        let token_file_arg = token_file.to_string_lossy().into_owned();

        let args = Args::parse_from(["termrelay", "--setup-token-file", &token_file_arg]).unwrap();

        assert_eq!(args.setup_token.as_deref(), Some("relay-setup-secret-1"));
        let rendered = format!("{args:?}");
        assert!(!rendered.contains("relay-setup-secret-1"));
        assert!(!rendered.contains(&token_file_arg));
        let _ = fs::remove_file(token_file);
    }

    #[test]
    fn parses_hashed_daemon_registry_file_without_plaintext_token() {
        let registry_file = write_temp_registry(
            r#"{"daemons":[{"server_id":"00000000-0000-0000-0000-000000000124","token_hash":"sha256:abc123"}]}"#,
        );
        let registry_arg = registry_file.to_string_lossy().into_owned();

        let args = Args::parse_from(["termrelay", "--daemon-registry", &registry_arg]).unwrap();

        assert_eq!(args.daemon_registry.daemons.len(), 1);
        assert_eq!(
            matches!(
                args.daemon_registry.daemons[0].runtime_credential(),
                Some(DaemonRegistryRuntimeCredential::TokenHash("sha256:abc123"))
            ),
            true
        );
        assert!(!format!("{args:?}").contains("sha256:abc123"));
        let _ = fs::remove_file(registry_file);
    }

    #[test]
    fn parses_missing_daemon_registry_as_empty_writable_registry_path() {
        let registry_file = std::env::temp_dir().join(format!(
            "termrelay-missing-daemon-registry-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_file(&registry_file);
        let registry_arg = registry_file.to_string_lossy().into_owned();

        let args = Args::parse_from(["termrelay", "--daemon-registry", &registry_arg]).unwrap();

        assert!(args.daemon_registry.daemons.is_empty());
        assert_eq!(args.daemon_registry_path, Some(registry_file));
    }

    #[test]
    fn rejects_blank_auth_token_file() {
        let token_file = write_temp_auth_token("  \n");
        let token_file_arg = token_file.to_string_lossy().into_owned();

        let error =
            Args::parse_from(["termrelay", "--auth-token-file", &token_file_arg]).unwrap_err();

        assert_eq!(error, ArgsError::EmptyValue("--auth-token-file"));
        let _ = fs::remove_file(token_file);
    }

    #[test]
    fn rejects_conflicting_auth_token_sources() {
        let token_file = write_temp_auth_token("relay-secret-from-file\n");
        let token_file_arg = token_file.to_string_lossy().into_owned();

        let error = Args::parse_from([
            "termrelay",
            "--auth-token",
            "relay-secret-inline",
            "--auth-token-file",
            &token_file_arg,
        ])
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--auth-token and --auth-token-file")
        );
        let _ = fs::remove_file(token_file);
    }

    #[test]
    fn rejects_file_first_auth_token_conflict_before_reading_file() {
        let missing_token_file =
            std::env::temp_dir().join("termrelay-auth-token-test-missing-conflict.txt");
        let missing_token_file_arg = missing_token_file.to_string_lossy().into_owned();

        let error = Args::parse_from([
            "termrelay",
            "--auth-token-file",
            &missing_token_file_arg,
            "--auth-token",
            "relay-secret-inline",
        ])
        .unwrap_err();

        assert_eq!(error, ArgsError::ConflictingAuthTokenSources);
    }

    #[test]
    fn rejects_equals_file_first_auth_token_conflict_before_reading_file() {
        let missing_token_file =
            std::env::temp_dir().join("termrelay-auth-token-test-missing-equals-conflict.txt");
        let missing_token_file_arg = missing_token_file.to_string_lossy().into_owned();
        let token_file_argument = format!("--auth-token-file={missing_token_file_arg}");

        let error = Args::parse_from([
            "termrelay",
            &token_file_argument,
            "--auth-token=relay-secret-inline",
        ])
        .unwrap_err();

        assert_eq!(error, ArgsError::ConflictingAuthTokenSources);
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
    fn parses_http_tunnel_flag() {
        let args = Args::parse_from(["termrelay", "--http-tunnel"]).unwrap();

        assert!(args.http_tunnel);
    }

    #[test]
    fn parses_help_and_version_without_requiring_server_start() {
        assert_eq!(
            RelayCommand::parse_from(["termrelay", "--help"]).unwrap(),
            RelayCommand::Help
        );
        assert_eq!(
            RelayCommand::parse_from(["termrelay", "-h"]).unwrap(),
            RelayCommand::Help
        );
        assert_eq!(
            RelayCommand::parse_from(["termrelay", "--version"]).unwrap(),
            RelayCommand::Version
        );
        assert_eq!(
            RelayCommand::parse_from(["termrelay", "-V"]).unwrap(),
            RelayCommand::Version
        );
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
    fn rejects_missing_auth_token_file_value() {
        let error = Args::parse_from(["termrelay", "--auth-token-file"]).unwrap_err();

        assert_eq!(error, ArgsError::MissingValue("--auth-token-file"));
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

    fn write_temp_auth_token(contents: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "termrelay-auth-token-test-{}-{unique}.txt",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    fn write_temp_registry(contents: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "termrelay-daemon-registry-test-{}-{unique}.json",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        path
    }
}
