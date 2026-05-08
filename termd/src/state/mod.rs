//! termd daemon 的本地持久状态快照。
//!
//! 本模块保存 daemon 需要跨进程重启保留的最小事实：daemon 公共身份快照、可信设备清单、
//! session 元数据，以及独立的 SQLite client history 存储入口。这里刻意不保存 PTY 明文
//! 输出、terminal 历史或文件传输内容，也不引入账号体系。

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use termd_proto::{
    DeviceId, PublicKey, ServerId, SessionId, SessionState, TerminalSize, UnixTimestampMillis,
};

pub mod client_history;

/// 当前 daemon 状态文件的 schema 版本。
pub const STATE_SCHEMA_VERSION: u32 = 1;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// daemon 可公开身份的持久快照。
///
/// 这里只保存 server id 和 daemon public key。server private key 不属于该快照，也不得写入
/// client 或 relay 可读的位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentitySnapshot {
    pub server_id: ServerId,
    pub public_key: PublicKey,
}

/// 已配对设备的持久状态记录。
///
/// 该结构只表达设备级信任事实；它不是账号或平台策略。controller/viewer 仍由 session
/// attach 规则在运行时决定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDeviceState {
    pub device_id: DeviceId,
    pub public_key: PublicKey,
    pub trusted_at_ms: UnixTimestampMillis,
    pub last_seen_at_ms: Option<UnixTimestampMillis>,
    pub label: Option<String>,
}

/// session 的最小持久元数据。
///
/// 记录只保存 session id、状态、尺寸和时间戳等恢复索引所需信息，不保存 PTY 输出、用户输入或
/// terminal 明文历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStateRecord {
    pub session_id: SessionId,
    pub state: SessionState,
    pub size: TerminalSize,
    pub created_at_ms: UnixTimestampMillis,
    pub updated_at_ms: UnixTimestampMillis,
    pub controller_device_id: Option<DeviceId>,
}

/// daemon 本地持久状态。
///
/// `version` 是 schema 迁移锚点；MVP 中 load/save 只接受当前结构，后续需要兼容旧版本时可以在
/// store 层增加迁移逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    pub version: u32,
    pub daemon_identity: Option<DaemonIdentitySnapshot>,
    pub trusted_devices: Vec<TrustedDeviceState>,
    pub sessions: Vec<SessionStateRecord>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            version: STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: Vec::new(),
        }
    }
}

/// daemon 状态文件读写入口。
#[derive(Debug, Default)]
pub struct StateStore;

impl StateStore {
    /// 从 JSON 状态文件读取 `DaemonState`。
    ///
    /// 缺失状态文件表示 daemon 第一次启动或还未保存任何本地状态，因此返回空的默认状态。
    pub fn load(path: impl AsRef<Path>) -> Result<DaemonState, StateError> {
        let path = path.as_ref();

        match fs::read_to_string(path) {
            Ok(raw) => parse_json(path, &raw),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DaemonState::default()),
            Err(source) => Err(StateError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// 将 daemon 状态以 JSON 保存到本地文件。
    ///
    /// 写入使用“临时文件 + rename”模式，保证目标文件要么保留旧内容，要么替换为完整新 JSON。
    pub fn save(path: impl AsRef<Path>, state: &DaemonState) -> Result<(), StateError> {
        write_json_atomically(path.as_ref(), state)
    }
}

/// 状态存储的结构化错误。
#[derive(Debug)]
pub enum StateError {
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
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize { path, .. } => {
                write!(f, "failed to serialize daemon state for {}", path.display())
            }
            Self::Read { path, .. } => {
                write!(f, "failed to read daemon state from {}", path.display())
            }
            Self::Parse { path, .. } => {
                write!(
                    f,
                    "failed to parse daemon state JSON from {}",
                    path.display()
                )
            }
            Self::CreateDirectory { path, .. } => {
                write!(f, "failed to create state directory {}", path.display())
            }
            Self::WriteTemp { path, .. } => {
                write!(f, "failed to write temporary state file {}", path.display())
            }
            Self::Rename { from, to, .. } => write!(
                f,
                "failed to atomically replace state file {} with {}",
                to.display(),
                from.display()
            ),
            Self::Sqlite { path, .. } => {
                write!(f, "failed to access sqlite store at {}", path.display())
            }
        }
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialize { source, .. } | Self::Parse { source, .. } => Some(source),
            Self::Read { source, .. }
            | Self::CreateDirectory { source, .. }
            | Self::WriteTemp { source, .. }
            | Self::Rename { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
        }
    }
}

fn parse_json<T>(path: &Path, raw: &str) -> Result<T, StateError>
where
    T: DeserializeOwned,
{
    serde_json::from_str(raw).map_err(|source| StateError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomically<T>(path: &Path, value: &T) -> Result<(), StateError>
where
    T: Serialize,
{
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| StateError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');

    ensure_parent_directory(path)?;

    let temp_path = temp_path_for(path);
    let result = write_temp_then_rename(path, &temp_path, &bytes);

    if result.is_err() {
        // 清理失败的临时文件不影响主错误返回；目标文件由 rename 原子性保护。
        fs::remove_file(&temp_path).ok();
    }

    result
}

fn ensure_parent_directory(path: &Path) -> Result<(), StateError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    fs::create_dir_all(parent).map_err(|source| StateError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })
}

fn write_temp_then_rename(path: &Path, temp_path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .map_err(|source| StateError::WriteTemp {
            path: temp_path.to_path_buf(),
            source,
        })?;

    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| StateError::WriteTemp {
            path: temp_path.to_path_buf(),
            source,
        })?;
    drop(file);

    fs::rename(temp_path, path).map_err(|source| StateError::Rename {
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
        .unwrap_or_else(|| "state".into());

    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        counter
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use termd_proto::{
        DeviceId, PublicKey, ServerId, SessionId, SessionState, TerminalSize, UnixTimestampMillis,
    };

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "termd-state-test-{}-{}-{name}",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn default_state_has_schema_version_and_no_runtime_content() {
        let state = DaemonState::default();

        assert_eq!(state.version, STATE_SCHEMA_VERSION);
        assert!(state.daemon_identity.is_none());
        assert!(state.trusted_devices.is_empty());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn trusted_device_state_roundtrips_json() {
        let device = TrustedDeviceState {
            device_id: DeviceId::new(),
            public_key: PublicKey("device-public".to_owned()),
            trusted_at_ms: UnixTimestampMillis(1000),
            last_seen_at_ms: Some(UnixTimestampMillis(2000)),
            label: Some("laptop".to_owned()),
        };

        let json = serde_json::to_string(&device).unwrap();
        let decoded: TrustedDeviceState = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, device);
    }

    #[test]
    fn state_store_saves_and_loads_full_state() {
        let state_path = temp_path("daemon-state.json");
        let state = sample_state();

        StateStore::save(&state_path, &state).unwrap();

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded, state);
        fs::remove_file(state_path).ok();
    }

    #[test]
    fn missing_state_load_returns_empty_default_state() {
        let state_path = temp_path("missing-state.json");

        let loaded = StateStore::load(&state_path).unwrap();

        assert_eq!(loaded, DaemonState::default());
    }

    #[test]
    fn corrupted_state_json_returns_structured_error() {
        let state_path = temp_path("corrupted-state.json");
        fs::write(&state_path, "{not-json").unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(error, StateError::Parse { .. }));
        fs::remove_file(state_path).ok();
    }

    #[test]
    fn state_save_uses_complete_json_target_after_atomic_write() {
        let state_path = temp_path("atomic-state.json");
        let state = sample_state();

        StateStore::save(&state_path, &state).unwrap();

        let raw = fs::read_to_string(&state_path).unwrap();
        let decoded: DaemonState = serde_json::from_str(&raw).unwrap();
        assert_eq!(decoded, state);
        assert!(raw.contains("\"version\""));
        fs::remove_file(state_path).ok();
    }

    fn sample_state() -> DaemonState {
        let device_id = DeviceId::new();

        DaemonState {
            version: STATE_SCHEMA_VERSION,
            daemon_identity: Some(DaemonIdentitySnapshot {
                server_id: ServerId::new(),
                public_key: PublicKey("daemon-public".to_owned()),
            }),
            trusted_devices: vec![TrustedDeviceState {
                device_id,
                public_key: PublicKey("device-public".to_owned()),
                trusted_at_ms: UnixTimestampMillis(1000),
                last_seen_at_ms: Some(UnixTimestampMillis(2000)),
                label: Some("laptop".to_owned()),
            }],
            sessions: vec![SessionStateRecord {
                session_id: SessionId::new(),
                state: SessionState::Running,
                size: TerminalSize::new(40, 120),
                created_at_ms: UnixTimestampMillis(3000),
                updated_at_ms: UnixTimestampMillis(4000),
                controller_device_id: Some(device_id),
            }],
        }
    }
}
