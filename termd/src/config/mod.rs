//! termd daemon 的本地配置文件存储。
//!
//! 本模块只保存单机 daemon 的本地启动配置：监听地址、状态文件路径、默认 shell/command、
//! pairing token 默认 TTL，以及 relay 连接所需的生产基线参数。它不是用户配置中心，
//! 也不表达平台级策略。

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::net::relay::RelayBaseUrl;

/// 当前配置 JSON 的 schema 版本。
///
/// 版本字段随文件一起持久化，后续需要迁移配置时可以先基于该字段做兼容判断。
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// 缺失配置文件时派生出的默认状态文件名。
pub const DEFAULT_STATE_FILE_NAME: &str = "daemon-state.json";

/// MVP 默认只监听本机，避免无意把未接入完整认证的 daemon 暴露到局域网。
pub const DEFAULT_LISTEN_HOST: &str = "127.0.0.1";

/// 本地 daemon 的默认 HTTP/WebSocket 端口。
pub const DEFAULT_LISTEN_PORT: u16 = 8765;

/// pairing token 的默认有效期：5 分钟。
pub const DEFAULT_PAIRING_TOKEN_TTL_MS: u64 = 5 * 60 * 1000;

/// daemon 主动连接 relay 的默认初始退避。
pub const DEFAULT_RELAY_RECONNECT_INITIAL_DELAY_MS: u64 = 250;

/// daemon 主动连接 relay 的默认最大退避。
pub const DEFAULT_RELAY_RECONNECT_MAX_DELAY_MS: u64 = 5_000;

/// relay mux socket 的默认心跳间隔。
pub const DEFAULT_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 15_000;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 本地配置中的敏感字符串。
///
/// 配置文件仍需要能保存 relay 凭证，但 Debug 输出必须脱敏，避免未来日志中误打印明文。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(\"<redacted>\")")
    }
}

/// daemon 主动连接 relay 时使用的重连与心跳策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayReconnectConfig {
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub heartbeat_interval_ms: u64,
}

impl Default for RelayReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: DEFAULT_RELAY_RECONNECT_INITIAL_DELAY_MS,
            max_delay_ms: DEFAULT_RELAY_RECONNECT_MAX_DELAY_MS,
            heartbeat_interval_ms: DEFAULT_RELAY_HEARTBEAT_INTERVAL_MS,
        }
    }
}

/// relay endpoint 规范化后的错误。
///
/// 这个错误只用于把本地配置和 CLI 中的 relay endpoint 列表收敛成稳定的 canonical 形式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEndpointError {
    Empty,
    Invalid { endpoint: String },
}

impl fmt::Display for RelayEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "relay endpoint cannot be empty"),
            Self::Invalid { endpoint } => write!(
                f,
                "invalid relay endpoint `{endpoint}`; expected ws://host:port or wss://host:port"
            ),
        }
    }
}

impl Error for RelayEndpointError {}

/// 把 relay endpoint 列表收敛成 canonical、去重后的顺序列表。
///
/// 这个 helper 只保留 `ws://host[:port]` / `wss://host[:port]` 级别的公开 endpoint，
/// 可带 relay base path `/ws`；不会把 query、fragment 或空值混进最终 supervisor 列表。
pub fn normalize_relay_endpoints(
    endpoints: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, RelayEndpointError> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();

    for endpoint in endpoints {
        let trimmed = endpoint.trim();
        if trimmed.is_empty() {
            return Err(RelayEndpointError::Empty);
        }

        let base = RelayBaseUrl::parse(trimmed).map_err(|_| RelayEndpointError::Invalid {
            endpoint: trimmed.to_owned(),
        })?;
        let canonical = base.canonical_url();
        if seen.insert(canonical.clone()) {
            normalized.push(canonical);
        }
    }

    Ok(normalized)
}

/// daemon 本地配置。
///
/// `DaemonConfig` 只描述 daemon 自己如何启动和在哪里保存本地状态；它不会把设备信任、
/// operator 状态或 session 生命周期编码成平台策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// 配置文件 schema 版本，便于后续做迁移。
    pub version: u32,
    /// daemon HTTP/WebSocket 监听 host。
    pub listen_host: String,
    /// daemon HTTP/WebSocket 监听端口。
    pub listen_port: u16,
    /// daemon 持久状态文件路径。
    pub state_path: PathBuf,
    /// 默认交互 shell，通常来自 `$SHELL`，缺失时回退到平台默认值。
    pub default_shell: String,
    /// 新建 PTY session 时的默认命令；MVP 中默认等于 `[default_shell]`。
    pub default_command: Vec<String>,
    /// 新建 PTY session 的默认工作目录；默认来自 daemon 运行用户的 `$HOME`。
    #[serde(default = "default_working_directory")]
    pub default_working_directory: Option<PathBuf>,
    /// 新 pairing token 的默认 TTL，单位为毫秒。
    pub pairing_token_ttl_ms: u64,
    /// daemon 主动连接的 relay endpoint 列表；为空时不自动连接 relay。
    #[serde(default)]
    pub relay_endpoints: Vec<String>,
    /// relay 访问凭证；它只认证 relay transport，不表达 session 控制权。
    #[serde(default)]
    pub relay_auth_token: Option<SecretString>,
    /// relay 自动重连和心跳策略。
    #[serde(default)]
    pub relay_reconnect: RelayReconnectConfig,
    /// `termd pair --qr` 生成 pairing URI 时默认写入的 WebSocket URL。
    #[serde(default = "default_pairing_ws_url")]
    pub default_pairing_ws_url: String,
}

impl DaemonConfig {
    /// 基于指定状态文件路径构造默认配置。
    ///
    /// 该构造函数用于首次启动或配置文件缺失时生成保守默认值；调用方仍可在保存前覆盖字段。
    pub fn default_for_state_path(path: impl Into<PathBuf>) -> Self {
        let default_shell = default_shell();

        Self {
            version: CONFIG_SCHEMA_VERSION,
            listen_host: DEFAULT_LISTEN_HOST.to_owned(),
            listen_port: DEFAULT_LISTEN_PORT,
            state_path: path.into(),
            default_shell: default_shell.clone(),
            default_command: vec![default_shell],
            default_working_directory: default_working_directory(),
            pairing_token_ttl_ms: DEFAULT_PAIRING_TOKEN_TTL_MS,
            relay_endpoints: Vec::new(),
            relay_auth_token: None,
            relay_reconnect: RelayReconnectConfig::default(),
            default_pairing_ws_url: default_pairing_ws_url(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self::default_for_state_path(DEFAULT_STATE_FILE_NAME)
    }
}

/// 配置文件读写入口。
///
/// 这里使用 unit struct 暴露命名空间式 API，后续如果需要挂载锁或迁移策略，可以在不改调用点
/// 的前提下扩展为有字段的 store。
#[derive(Debug, Default)]
pub struct ConfigStore;

impl ConfigStore {
    /// 从 JSON 配置文件读取 `DaemonConfig`。
    ///
    /// 缺失文件返回基于配置文件目录派生出的默认配置，而不是 panic 或创建半成品文件。这个行为
    /// 在测试中固定下来，便于 daemon 首次启动时由上层决定是否立刻保存默认配置。
    pub fn load(path: impl AsRef<Path>) -> Result<DaemonConfig, ConfigError> {
        let path = path.as_ref();

        match fs::read_to_string(path) {
            Ok(raw) => parse_json(path, &raw),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(
                DaemonConfig::default_for_state_path(default_state_path_for_config(path)),
            ),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// 将配置保存为 JSON。
    ///
    /// 保存流程先写入同目录临时文件，再通过 rename 替换目标文件，避免 daemon 崩溃时留下半截
    /// JSON。该函数只负责本地文件原子性，不做跨进程锁。
    pub fn save(path: impl AsRef<Path>, config: &DaemonConfig) -> Result<(), ConfigError> {
        write_json_atomically(path.as_ref(), config)
    }
}

/// 配置存储的结构化错误。
#[derive(Debug)]
pub enum ConfigError {
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    WriteTemp {
        path: PathBuf,
        source: io::Error,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize { path, .. } => {
                write!(
                    f,
                    "failed to serialize daemon config for {}",
                    path.display()
                )
            }
            Self::Read { path, .. } => {
                write!(f, "failed to read daemon config from {}", path.display())
            }
            Self::Parse { path, .. } => {
                write!(
                    f,
                    "failed to parse daemon config JSON from {}",
                    path.display()
                )
            }
            Self::CreateDirectory { path, .. } => {
                write!(f, "failed to create config directory {}", path.display())
            }
            Self::WriteTemp { path, .. } => {
                write!(
                    f,
                    "failed to write temporary config file {}",
                    path.display()
                )
            }
            Self::Rename { from, to, .. } => write!(
                f,
                "failed to atomically replace config file {} with {}",
                to.display(),
                from.display()
            ),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialize { source, .. } | Self::Parse { source, .. } => Some(source),
            Self::Read { source, .. }
            | Self::CreateDirectory { source, .. }
            | Self::WriteTemp { source, .. }
            | Self::Rename { source, .. } => Some(source),
        }
    }
}

fn parse_json<T>(path: &Path, raw: &str) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
{
    serde_json::from_str(raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomically<T>(path: &Path, value: &T) -> Result<(), ConfigError>
where
    T: Serialize,
{
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');

    ensure_parent_directory(path)?;

    let temp_path = temp_path_for(path);
    let result = write_temp_then_rename(path, &temp_path, &bytes);

    if result.is_err() {
        // 失败后尽量清理临时文件；清理失败不覆盖原始错误，避免隐藏真正的 I/O 原因。
        fs::remove_file(&temp_path).ok();
    }

    result
}

fn ensure_parent_directory(path: &Path) -> Result<(), ConfigError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })
}

fn write_temp_then_rename(path: &Path, temp_path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .map_err(|source| ConfigError::WriteTemp {
            path: temp_path.to_path_buf(),
            source,
        })?;

    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| ConfigError::WriteTemp {
            path: temp_path.to_path_buf(),
            source,
        })?;
    drop(file);

    fs::rename(temp_path, path).map_err(|source| ConfigError::Rename {
        from: temp_path.to_path_buf(),
        to: path.to_path_buf(),
        source,
    })
}

fn temp_path_for(path: &Path) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "config".into());

    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        counter
    ))
}

fn default_state_path_for_config(config_path: &Path) -> PathBuf {
    match config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent.join(DEFAULT_STATE_FILE_NAME),
        None => PathBuf::from(DEFAULT_STATE_FILE_NAME),
    }
}

fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|shell| shell.trim().to_owned())
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(platform_default_shell)
}

fn platform_default_shell() -> String {
    if cfg!(windows) {
        "cmd.exe".to_owned()
    } else {
        "/bin/sh".to_owned()
    }
}

fn default_working_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn default_pairing_ws_url() -> String {
    format!("ws://{DEFAULT_LISTEN_HOST}:{DEFAULT_LISTEN_PORT}/ws")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "termd-config-test-{}-{}-{name}",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn default_config_for_state_path_sets_mvp_fields() {
        let state_path = temp_path("state.json");

        let config = DaemonConfig::default_for_state_path(&state_path);

        assert_eq!(config.version, CONFIG_SCHEMA_VERSION);
        assert_eq!(config.listen_host, "127.0.0.1");
        assert!(config.listen_port > 0);
        assert_eq!(config.state_path, state_path);
        assert!(!config.default_shell.is_empty());
        assert_eq!(config.default_command, vec![config.default_shell.clone()]);
        assert_eq!(
            config.default_working_directory,
            default_working_directory()
        );
        assert!(config.pairing_token_ttl_ms > 0);
        assert!(config.relay_endpoints.is_empty());
        assert!(config.relay_auth_token.is_none());
        assert_eq!(config.default_pairing_ws_url, "ws://127.0.0.1:8765/ws");
        assert!(config.relay_reconnect.initial_delay_ms > 0);
        assert!(config.relay_reconnect.max_delay_ms >= config.relay_reconnect.initial_delay_ms);
        assert!(config.relay_reconnect.heartbeat_interval_ms > 0);
    }

    #[test]
    fn normalize_relay_endpoints_deduplicates_and_canonicalizes() {
        let normalized = normalize_relay_endpoints(vec![
            " ws://127.0.0.1:8080/ ".to_owned(),
            "ws://127.0.0.1:8080".to_owned(),
            "wss://termd.yiln.de/ws/".to_owned(),
            "wss://termd.yiln.de/ws".to_owned(),
            "wss://relay.example:443".to_owned(),
            "wss://relay.example:443/".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            normalized,
            vec![
                "ws://127.0.0.1:8080".to_owned(),
                "wss://termd.yiln.de/ws".to_owned(),
                "wss://relay.example:443".to_owned(),
            ]
        );
    }

    #[test]
    fn normalize_relay_endpoints_rejects_empty_and_invalid() {
        assert!(matches!(
            normalize_relay_endpoints(vec!["   ".to_owned()]).unwrap_err(),
            RelayEndpointError::Empty
        ));

        assert!(matches!(
            normalize_relay_endpoints(vec!["http://127.0.0.1:8080".to_owned()]).unwrap_err(),
            RelayEndpointError::Invalid { endpoint } if endpoint == "http://127.0.0.1:8080"
        ));
    }

    #[test]
    fn config_store_saves_and_loads_full_config() {
        let config_path = temp_path("config.json");
        let state_path = temp_path("state.json");
        let mut config = DaemonConfig::default_for_state_path(&state_path);
        config.listen_host = "0.0.0.0".to_owned();
        config.listen_port = 9001;
        config.default_shell = "/bin/zsh".to_owned();
        config.default_command = vec!["/bin/zsh".to_owned(), "-l".to_owned()];
        config.default_working_directory = Some(PathBuf::from("/home/termd"));
        config.pairing_token_ttl_ms = 42_000;
        config.relay_endpoints = vec!["ws://127.0.0.1:8080".to_owned()];
        config.relay_auth_token = Some(SecretString::new("relay-secret-1"));
        config.relay_reconnect = RelayReconnectConfig {
            initial_delay_ms: 250,
            max_delay_ms: 10_000,
            heartbeat_interval_ms: 15_000,
        };
        config.default_pairing_ws_url = "ws://relay.example/ws/server/client".to_owned();

        ConfigStore::save(&config_path, &config).unwrap();

        let loaded = ConfigStore::load(&config_path).unwrap();
        assert_eq!(loaded, config);
        let raw = fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains("relay-secret-1"));
        assert!(!format!("{config:?}").contains("relay-secret-1"));
        fs::remove_file(config_path).ok();
    }

    #[test]
    fn missing_config_load_returns_default_with_derived_state_path() {
        let config_path = temp_path("missing-config.json");

        let loaded = ConfigStore::load(&config_path).unwrap();

        assert_eq!(
            loaded.state_path,
            config_path.with_file_name(DEFAULT_STATE_FILE_NAME)
        );
    }

    #[test]
    fn corrupted_config_json_returns_structured_error() {
        let config_path = temp_path("corrupted-config.json");
        fs::write(&config_path, "{not-json").unwrap();

        let error = ConfigStore::load(&config_path).unwrap_err();

        assert!(matches!(error, ConfigError::Parse { .. }));
        fs::remove_file(config_path).ok();
    }

    #[test]
    fn config_save_uses_complete_json_target_after_atomic_write() {
        let config_path = temp_path("atomic-config.json");
        let config = DaemonConfig::default_for_state_path(temp_path("state.json"));

        ConfigStore::save(&config_path, &config).unwrap();

        let raw = fs::read_to_string(&config_path).unwrap();
        let decoded: DaemonConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(decoded, config);
        assert!(raw.contains("\"version\""));
        fs::remove_file(config_path).ok();
    }
}
