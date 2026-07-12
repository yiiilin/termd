use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;

use serde::Deserialize;
use termd_proto::{PublicKey, ServerId};
use thiserror::Error;

/// 公网部署时通常只绑定内网或 loopback，再由反向代理对外提供 WSS。
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

#[derive(Clone, PartialEq, Eq)]
pub struct Args {
    /// relay 的内部监听地址；公网入口通常交给反向代理，不直接暴露这里的端口。
    pub listen: SocketAddr,
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
    #[serde(default)]
    pub daemon_public_key: Option<PublicKey>,
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
            .field(
                "daemon_public_key_configured",
                &self.daemon_public_key.is_some(),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayCommand {
    Serve(Args),
    Help,
    Version,
}

impl fmt::Debug for Args {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Args")
            .field("listen", &self.listen)
            .field("setup_token_configured", &self.setup_token.is_some())
            .field("tls_cert", &self.tls_cert)
            .field("tls_key_configured", &self.tls_key.is_some())
            .field("daemon_registry_count", &self.daemon_registry.daemons.len())
            .field("allow_open_relay", &self.allow_open_relay)
            .field("web", &self.web)
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
    #[error("failed to read setup token file")]
    ReadSetupTokenFile,
    #[error("failed to read daemon registry file")]
    ReadDaemonRegistryFile,
    #[error("failed to parse daemon registry file")]
    ParseDaemonRegistryFile,
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
        let mut setup_token_file = None;
        let mut tls_cert = None;
        let mut tls_key = None;
        let mut daemon_registry_file = None;
        let mut allow_open_relay = false;
        let mut web = false;
        let mut args = args.into_iter().map(Into::into);

        // 第一个参数是程序名；不要求存在，方便单元测试直接传空迭代器。
        let _program = args.next();

        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy().into_owned();
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
                other => return Err(ArgsError::UnknownArgument(other.to_owned())),
            }
        }

        if tls_cert.is_some() != tls_key.is_some() {
            return Err(ArgsError::IncompleteTlsConfig);
        }

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
            setup_token,
            tls_cert,
            tls_key,
            daemon_registry,
            daemon_registry_path,
            allow_open_relay,
            web,
        })
    }
}

fn non_empty_path(flag: &'static str, value: &str) -> Result<PathBuf, ArgsError> {
    if value.is_empty() {
        return Err(ArgsError::EmptyValue(flag));
    }
    Ok(PathBuf::from(value))
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
    #[cfg(any())]
    fn parses_auth_token_without_debug_leakage() {
        let args = Args::parse_from(["termrelay", "--auth-token", "relay-secret-1"]).unwrap();

        assert_eq!(args.auth_token.as_deref(), Some("relay-secret-1"));
        assert!(!format!("{args:?}").contains("relay-secret-1"));
    }

    #[test]
    #[cfg(any())]
    fn parses_equals_auth_token_without_debug_leakage() {
        let args = Args::parse_from(["termrelay", "--auth-token=relay-secret-equals"]).unwrap();

        assert_eq!(args.auth_token.as_deref(), Some("relay-secret-equals"));
        assert!(!format!("{args:?}").contains("relay-secret-equals"));
    }

    #[test]
    #[cfg(any())]
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
    #[cfg(any())]
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
        assert!(matches!(
            args.daemon_registry.daemons[0].runtime_credential(),
            Some(DaemonRegistryRuntimeCredential::TokenHash("sha256:abc123"))
        ));
        assert!(!format!("{args:?}").contains("sha256:abc123"));
        let _ = fs::remove_file(registry_file);
    }

    #[test]
    fn v070_parses_daemon_public_key_from_registry() {
        let registry_file = write_temp_registry(
            r#"{"daemons":[{"server_id":"00000000-0000-0000-0000-000000000070","token_hash":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","daemon_public_key":"ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}]}"#,
        );
        let args = Args::parse_from([
            "termrelay",
            "--daemon-registry",
            registry_file.to_str().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            args.daemon_registry.daemons[0]
                .daemon_public_key
                .as_ref()
                .unwrap()
                .0,
            "ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
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
    #[cfg(any())]
    fn rejects_blank_auth_token_file() {
        let token_file = write_temp_auth_token("  \n");
        let token_file_arg = token_file.to_string_lossy().into_owned();

        let error =
            Args::parse_from(["termrelay", "--auth-token-file", &token_file_arg]).unwrap_err();

        assert_eq!(error, ArgsError::EmptyValue("--auth-token-file"));
        let _ = fs::remove_file(token_file);
    }

    #[test]
    #[cfg(any())]
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
    #[cfg(any())]
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
    #[cfg(any())]
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
    #[cfg(any())]
    fn rejects_missing_auth_token_value() {
        let error = Args::parse_from(["termrelay", "--auth-token"]).unwrap_err();

        assert_eq!(error, ArgsError::MissingValue("--auth-token"));
    }

    #[test]
    #[cfg(any())]
    fn rejects_empty_auth_token_value() {
        let error = Args::parse_from(["termrelay", "--auth-token", ""]).unwrap_err();

        assert_eq!(error, ArgsError::EmptyValue("--auth-token"));
    }

    #[test]
    #[cfg(any())]
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
