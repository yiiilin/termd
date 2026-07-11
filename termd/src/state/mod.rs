//! termd daemon 的本地持久状态快照。
//!
//! 本模块保存 daemon 需要跨进程重启保留的最小事实：daemon 公共身份快照、可信设备清单、
//! session 元数据，以及 SQLite client history 存储入口。这里刻意不保存 PTY 明文输出、
//! terminal 历史或文件传输内容，也不引入账号体系。

use rusqlite::OpenFlags;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::error::Error;
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
    io::{AsRawFd, FromRawFd},
};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use termd_proto::{
    DeviceId, PublicKey, ServerId, SessionId, SessionState, TerminalSize, UnixTimestampMillis,
};
use uuid::Uuid;

use crate::pty::{PtyRestoreInfo, PtySupervisorStatus};

pub mod client_history;

/// 当前 daemon 状态文件的 schema 版本。
pub const STATE_SCHEMA_VERSION: u32 = 3;

const META_STATE_SCHEMA_VERSION: &str = "state_schema_version";
const META_SERVER_ID: &str = "server_id";
const META_DAEMON_PUBLIC_KEY: &str = "daemon_public_key";
const META_DAEMON_PRIVATE_KEY: &str = "daemon_private_key";
#[cfg(unix)]
const SQLITE_PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const SQLITE_PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// daemon 身份的本地持久快照。
///
/// private key 只允许写入 daemon 本地 SQLite；pair payload、termctl state 和 relay 都不能保存
/// 这个字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentitySnapshot {
    pub server_id: ServerId,
    pub public_key: PublicKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
}

/// 已配对设备的持久状态记录。
///
/// 该结构只表达设备级信任事实；它不是账号或平台策略。operator 状态仍由 session
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_info: Option<PtyRestoreInfo>,
}

/// 未完成 HTTP upload 的恢复清理记录。
///
/// 这里只保存同目录隐藏临时对象路径和创建时的文件 identity；daemon 重启后只用它做安全 cleanup，
/// 不保存任何文件内容或 E2EE 明文。Unix/Windows 下 `dev/ino` 是原生文件对象 ID；
/// Windows 缺失 file id 时用协议层 sentinel 表示；其他平台没有稳定 file id，cleanup
/// 会安全失败，不静默删除未知对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpUploadRecoveryRecord {
    pub upload_id: String,
    pub target_path: PathBuf,
    pub size_bytes: u64,
    pub dev: u64,
    pub ino: u64,
    pub updated_at_ms: UnixTimestampMillis,
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
    /// 检查当前 SQLite 状态库是否可以按本版本打开。
    ///
    /// 中文注释：v2 不迁移旧 supervisor restore state；因此只允许两种情况：
    /// 1. 已有 `state_schema_version = 2`；
    /// 2. 真正全空的新库，等待后续 v2 写入口初始化。
    pub fn ensure_compatible(path: impl AsRef<Path>) -> Result<(), StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let conn = open_state_connection(&sqlite_path)?;
        ensure_compatible_connection(&conn, &sqlite_path)
    }

    /// 从 SQLite 状态库读取 `DaemonState`。
    ///
    /// 旧版本的 `daemon-state.json` 只作为迁移来源读取；SQLite 一旦有 daemon 状态，
    /// 后续启动都以 SQLite 为准，避免 stale JSON 覆盖新信任数据。
    pub fn load(path: impl AsRef<Path>) -> Result<DaemonState, StateError> {
        let path = path.as_ref();
        #[cfg(not(unix))]
        {
            Err(StateError::UnsupportedPlatform {
                path: sqlite_state_path_for_state_path(path),
            })
        }
        #[cfg(unix)]
        {
            let path = absolute_state_path(path)?;
            let sqlite_path = sqlite_state_path_for_state_path(&path);
            let conn = open_state_connection(&sqlite_path)?;
            initialize_daemon_state_schema(&conn, &sqlite_path)?;
            ensure_sqlite_state_version(&conn, &sqlite_path)?;
            let state = load_sqlite_state(&conn, &sqlite_path)?;

            if state.daemon_identity.is_some()
                || !state.trusted_devices.is_empty()
                || !state.sessions.is_empty()
            {
                return Ok(state);
            }

            load_legacy_json_state(&path, &sqlite_path).map(|legacy| legacy.unwrap_or(state))
        }
    }

    /// 将 daemon 状态保存到 SQLite 状态库。
    ///
    /// 这里不再写 `daemon-state.json`；旧 JSON 文件即使存在，也只作为迁移来源。
    pub fn save(path: impl AsRef<Path>, state: &DaemonState) -> Result<(), StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let mut conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        save_sqlite_state(&mut conn, &sqlite_path, state)
    }

    /// 删除单个已关闭 runtime tombstone。调用方必须先确认对应 supervisor 已经结束。
    pub fn prune_closed_session(
        path: impl AsRef<Path>,
        session_id: SessionId,
    ) -> Result<bool, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        let deleted = conn
            .execute(
                "DELETE FROM runtime_sessions WHERE session_id = ?1 AND state = ?2",
                params![
                    session_id.0.to_string(),
                    session_state_text(SessionState::Closed)
                ],
            )
            .map_err(|source| sqlite_error(&sqlite_path, source))?
            > 0;
        Ok(deleted)
    }

    /// 显式记录 runtime session 已关闭，并清掉 supervisor 恢复信息。
    ///
    /// `save` 只保存当前快照，不再把“快照缺失”推断为关闭；调用方在确认 close 或恢复失败时
    /// 必须走这个显式 tombstone 路径，避免临时快照漏项把仍存活的 supervisor 标成 closed。
    pub fn record_runtime_session_closed(
        path: impl AsRef<Path>,
        session_id: SessionId,
        now_ms: UnixTimestampMillis,
    ) -> Result<bool, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let mut conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&sqlite_path, source))?;
        upsert_meta_value(
            &tx,
            &sqlite_path,
            META_STATE_SCHEMA_VERSION,
            &STATE_SCHEMA_VERSION.to_string(),
            now_ms.0 as i64,
        )?;
        tx.execute(
            r#"
                INSERT INTO runtime_sessions (
                    session_id,
                    state,
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                    created_at_ms,
                    updated_at_ms,
                    restore_kind,
                    restore_value
                )
                VALUES (?1, ?2, 1, 1, 0, 0, ?3, ?3, NULL, NULL)
                ON CONFLICT(session_id) DO UPDATE SET
                    state = excluded.state,
                    updated_at_ms = excluded.updated_at_ms,
                    restore_kind = NULL,
                    restore_value = NULL
                "#,
            params![
                session_id.0.to_string(),
                session_state_text(SessionState::Closed),
                now_ms.0 as i64,
            ],
        )
        .map_err(|source| sqlite_error(&sqlite_path, source))?;
        tx.commit()
            .map_err(|source| sqlite_error(&sqlite_path, source))?;
        Ok(true)
    }

    /// 清理已经不可恢复的 closed runtime 行；仍有 live supervisor 的 id 必须保留。
    pub fn prune_closed_sessions_except(
        path: impl AsRef<Path>,
        protected_session_ids: &HashSet<SessionId>,
    ) -> Result<usize, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let mut conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        prune_closed_runtime_sessions_except(&mut conn, &sqlite_path, protected_session_ids)
    }

    pub fn list_http_uploads(
        path: impl AsRef<Path>,
    ) -> Result<Vec<HttpUploadRecoveryRecord>, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        list_http_uploads(&conn, &sqlite_path)
    }

    pub fn record_http_upload(
        path: impl AsRef<Path>,
        record: &HttpUploadRecoveryRecord,
    ) -> Result<(), StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        write_sqlite_state_version(&conn, &sqlite_path)?;
        conn.execute(
            r#"
            INSERT INTO http_uploads (
                upload_id,
                target_path,
                size_bytes,
                dev,
                ino,
                updated_at_ms
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(upload_id) DO UPDATE SET
                target_path = excluded.target_path,
                size_bytes = excluded.size_bytes,
                dev = excluded.dev,
                ino = excluded.ino,
                updated_at_ms = excluded.updated_at_ms
            "#,
            params![
                record.upload_id,
                record.target_path.to_string_lossy().as_ref(),
                record.size_bytes.to_string(),
                record.dev.to_string(),
                record.ino.to_string(),
                record.updated_at_ms.0 as i64,
            ],
        )
        .map_err(|source| sqlite_error(&sqlite_path, source))?;
        Ok(())
    }

    pub fn remove_http_upload(path: impl AsRef<Path>, upload_id: &str) -> Result<bool, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        ensure_sqlite_state_version(&conn, &sqlite_path)?;
        let deleted = conn
            .execute(
                "DELETE FROM http_uploads WHERE upload_id = ?1",
                params![upload_id],
            )
            .map_err(|source| sqlite_error(&sqlite_path, source))?
            > 0;
        Ok(deleted)
    }
}

/// 状态存储的结构化错误。
#[derive(Debug)]
pub enum StateError {
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
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
    UnsafeSqlitePath {
        path: PathBuf,
        kind: UnsafeSqlitePathKind,
    },
    RestrictSqlitePermissions {
        path: PathBuf,
        source: io::Error,
    },
    ResolveRelativeSqlitePath {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedPlatform {
        path: PathBuf,
    },
    InvalidDaemonIdentity {
        source: String,
    },
    InvalidSupervisorCleanupCapability {
        session_id: SessionId,
    },
    InvalidOwnershipState {
        source: String,
    },
    IncompatibleVersion {
        path: PathBuf,
        found: Option<u32>,
        expected: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeSqlitePathKind {
    Symlink,
    NonRegular,
    NonDirectory,
    WrongOwner,
    InsecurePermissions,
    Replaced,
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::Sqlite { path, .. } => {
                write!(f, "failed to access sqlite store at {}", path.display())
            }
            Self::UnsafeSqlitePath { path, kind } => match kind {
                UnsafeSqlitePathKind::Symlink => {
                    write!(f, "refusing to open sqlite symlink at {}", path.display())
                }
                UnsafeSqlitePathKind::NonRegular => write!(
                    f,
                    "refusing to open non-regular sqlite path at {}",
                    path.display()
                ),
                UnsafeSqlitePathKind::NonDirectory => write!(
                    f,
                    "refusing to use non-directory sqlite parent at {}",
                    path.display()
                ),
                UnsafeSqlitePathKind::WrongOwner => write!(
                    f,
                    "refusing to use sqlite path not owned by the current user at {}",
                    path.display()
                ),
                UnsafeSqlitePathKind::InsecurePermissions => write!(
                    f,
                    "refusing to use non-private sqlite parent at {}",
                    path.display()
                ),
                UnsafeSqlitePathKind::Replaced => {
                    write!(f, "refusing replaced sqlite path at {}", path.display())
                }
            },
            Self::RestrictSqlitePermissions { path, .. } => write!(
                f,
                "failed to restrict sqlite file permissions at {}",
                path.display()
            ),
            Self::ResolveRelativeSqlitePath { path, .. } => write!(
                f,
                "failed to resolve relative sqlite store path {} against the current directory",
                path.display()
            ),
            Self::UnsupportedPlatform { path } => write!(
                f,
                "SQLite state storage is unsupported on this platform at {}",
                path.display()
            ),
            Self::InvalidDaemonIdentity { .. } => write!(f, "failed to restore daemon identity"),
            Self::InvalidSupervisorCleanupCapability { session_id } => write!(
                f,
                "session {session_id:?} has no valid supervisor cleanup capability"
            ),
            Self::InvalidOwnershipState { .. } => {
                write!(f, "failed to open durable session ownership state")
            }
            Self::IncompatibleVersion {
                path,
                found,
                expected,
            } => match found {
                Some(found) => write!(
                    f,
                    "daemon state at {} uses schema version {found}, expected {expected}; reset state before using the current supervisor runtime",
                    path.display()
                ),
                None => write!(
                    f,
                    "daemon state at {} was created before schema version {expected}; reset state before using the current supervisor runtime",
                    path.display()
                ),
            },
        }
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse { source, .. } => Some(source),
            Self::Read { source, .. }
            | Self::CreateDirectory { source, .. }
            | Self::RestrictSqlitePermissions { source, .. }
            | Self::ResolveRelativeSqlitePath { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            Self::InvalidOwnershipState { .. }
            | Self::UnsafeSqlitePath { .. }
            | Self::UnsupportedPlatform { .. }
            | Self::InvalidDaemonIdentity { .. }
            | Self::InvalidSupervisorCleanupCapability { .. }
            | Self::IncompatibleVersion { .. } => None,
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

/// 从历史 `state_path` 派生当前唯一 SQLite 状态库路径。
///
/// 保留 `.json -> .sqlite` 的派生规则是为了让旧配置无需改路径也能平滑迁移。
pub(crate) fn sqlite_state_path_for_state_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("sqlite")
}

fn load_legacy_json_state(
    path: &Path,
    sqlite_path: &Path,
) -> Result<Option<DaemonState>, StateError> {
    if path == sqlite_path {
        return Ok(None);
    }

    match fs::read_to_string(path) {
        Ok(raw) => {
            let state: DaemonState = parse_json(path, &raw)?;
            migrate_legacy_state(path, state).map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StateError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn open_state_connection(sqlite_path: &Path) -> Result<Connection, StateError> {
    #[cfg(not(unix))]
    {
        Err(StateError::UnsupportedPlatform {
            path: sqlite_path.to_path_buf(),
        })
    }
    #[cfg(unix)]
    {
        let sqlite_path = absolute_state_path(sqlite_path)?;
        let secure_open = prepare_secure_sqlite_file_for_open(&sqlite_path)?;
        #[cfg(test)]
        change_current_dir_after_secure_prepare_for_test();
        let conn = open_sqlite_connection(&sqlite_path)?;
        verify_secure_sqlite_open(&sqlite_path, &secure_open)?;

        // SQLite 管理 WAL/SHM 的内部创建与替换；私有且不可被其他 UID 写入的父目录
        // 是 sidecar 的边界。这里不声称抵御同 UID 或 root 进程，也不增加自定义 VFS。
        Ok(conn)
    }
}

pub(crate) fn open_private_state_connection(sqlite_path: &Path) -> Result<Connection, StateError> {
    open_state_connection(sqlite_path)
}

#[cfg(unix)]
fn absolute_state_path(path: &Path) -> Result<PathBuf, StateError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let current_dir =
        std::env::current_dir().map_err(|source| StateError::ResolveRelativeSqlitePath {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(current_dir.join(path))
}

#[cfg(all(test, unix))]
fn change_current_dir_after_secure_prepare_for_test() {
    if let Some(path) = std::env::var_os("TERMD_TEST_CWD_AFTER_SQLITE_PREPARE") {
        std::env::set_current_dir(path).expect("test hook must change the child process cwd");
    }
}

#[cfg(unix)]
fn open_sqlite_connection(sqlite_path: &Path) -> Result<Connection, StateError> {
    Connection::open_with_flags(sqlite_path, sqlite_open_flags())
        .map_err(|source| sqlite_error(sqlite_path, source))
}

fn sqlite_open_flags() -> OpenFlags {
    // 中文注释：预检查按普通 Path 校验，不能让 SQLite 再把 `file:` 解释成 URI。
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
}

#[cfg(unix)]
struct SecureSqliteOpen {
    database: fs::File,
    parent: fs::File,
    parent_path: PathBuf,
}

#[cfg(unix)]
fn prepare_secure_sqlite_file_for_open(sqlite_path: &Path) -> Result<SecureSqliteOpen, StateError> {
    let (parent_path, parent) = open_private_sqlite_parent(sqlite_path)?;
    let file = match create_secure_sqlite_file(&parent, sqlite_path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            open_sqlite_file_nofollow(&parent, sqlite_path)?
        }
        Err(source) => return Err(sqlite_file_open_error(sqlite_path, source)),
    };
    secure_opened_sqlite_file(sqlite_path, &file)?;
    secure_existing_sqlite_sidecars(sqlite_path, &parent)?;
    verify_path_identity(&parent_path, &parent, true)?;
    Ok(SecureSqliteOpen {
        database: file,
        parent,
        parent_path,
    })
}

#[cfg(unix)]
fn open_private_sqlite_parent(sqlite_path: &Path) -> Result<(PathBuf, fs::File), StateError> {
    let parent_path = sqlite_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let parent = open_or_create_directory_path_nofollow(&parent_path)?;

    let metadata = parent
        .metadata()
        .map_err(|source| sqlite_file_open_error(&parent_path, source))?;
    if !metadata.is_dir() {
        return Err(StateError::UnsafeSqlitePath {
            path: parent_path,
            kind: UnsafeSqlitePathKind::NonDirectory,
        });
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(StateError::UnsafeSqlitePath {
            path: parent_path,
            kind: UnsafeSqlitePathKind::WrongOwner,
        });
    }
    let mode = parent
        .metadata()
        .map_err(|source| sqlite_file_open_error(&parent_path, source))?
        .mode();
    if mode & 0o022 != 0 {
        return Err(StateError::UnsafeSqlitePath {
            path: parent_path,
            kind: UnsafeSqlitePathKind::InsecurePermissions,
        });
    }
    verify_path_identity(&parent_path, &parent, true)?;
    Ok((parent_path, parent))
}

#[cfg(unix)]
fn open_or_create_directory_path_nofollow(path: &Path) -> Result<fs::File, StateError> {
    let components = path.components().collect::<Vec<_>>();
    let (mut current, mut current_path) = if path.is_absolute() {
        (
            open_directory_nofollow(Path::new("/"))
                .map_err(|source| sqlite_file_open_error(Path::new("/"), source))?,
            PathBuf::from("/"),
        )
    } else {
        (
            open_directory_nofollow(Path::new("."))
                .map_err(|source| sqlite_file_open_error(Path::new("."), source))?,
            PathBuf::from("."),
        )
    };
    validate_sqlite_ancestor(&current_path, &current)?;

    for (index, component) in components.iter().enumerate() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => unreachable!("Unix paths do not have prefixes"),
        };
        let component_path = current_path.join(name);
        let is_final = components[index + 1..]
            .iter()
            .all(|component| matches!(component, Component::RootDir | Component::CurDir));
        let (next, created) = match open_directory_at_nofollow(&current, name) {
            Ok(next) => (next, false),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let created = match mkdirat_private(&current, name) {
                    Ok(()) => true,
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(source) => {
                        return Err(StateError::CreateDirectory {
                            path: component_path,
                            source,
                        });
                    }
                };
                (
                    open_directory_at_nofollow(&current, name)
                        .map_err(|source| sqlite_file_open_error(&component_path, source))?,
                    created,
                )
            }
            Err(source) => return Err(sqlite_file_open_error(&component_path, source)),
        };
        if created {
            fchmod_opened_path(&component_path, &next, SQLITE_PRIVATE_DIRECTORY_MODE)?;
        }
        if !is_final {
            validate_sqlite_ancestor(&component_path, &next)?;
        }
        current = next;
        current_path = component_path;
    }
    Ok(current)
}

#[cfg(unix)]
fn validate_sqlite_ancestor(path: &Path, directory: &fs::File) -> Result<(), StateError> {
    let metadata = directory
        .metadata()
        .map_err(|source| sqlite_file_open_error(path, source))?;
    if !metadata.is_dir() {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::NonDirectory,
        });
    }
    let mode = metadata.mode();
    let uid = metadata.uid();
    let current_uid = unsafe { libc::geteuid() };
    let owner_is_trusted = uid == current_uid || uid == 0;
    let foreign_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    if !owner_is_trusted || (foreign_writable && !sticky) {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::InsecurePermissions,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(unix)]
fn open_directory_at_nofollow(parent: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    openat_file(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

#[cfg(unix)]
fn mkdirat_private(parent: &fs::File, name: &OsStr) -> io::Result<()> {
    let name = path_component_cstring(name)?;
    // SAFETY: `parent` and `name` stay alive for the duration of mkdirat.
    let result = unsafe {
        libc::mkdirat(
            parent.as_raw_fd(),
            name.as_ptr(),
            SQLITE_PRIVATE_DIRECTORY_MODE as libc::mode_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn create_secure_sqlite_file(parent: &fs::File, path: &Path) -> io::Result<fs::File> {
    openat_file(
        parent,
        sqlite_file_name(path)?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        SQLITE_PRIVATE_FILE_MODE,
    )
}

#[cfg(unix)]
fn open_sqlite_file_nofollow(parent: &fs::File, path: &Path) -> Result<fs::File, StateError> {
    openat_file(
        parent,
        sqlite_file_name(path).map_err(|source| sqlite_file_open_error(path, source))?,
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|source| sqlite_file_open_error(path, source))
}

#[cfg(unix)]
fn openat_file(parent: &fs::File, name: &OsStr, flags: i32, mode: u32) -> io::Result<fs::File> {
    let name = path_component_cstring(name)?;
    // SAFETY: `parent` and `name` stay alive for openat; a successful call returns an owned FD.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a new owned file descriptor.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn path_component_cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(unix)]
fn sqlite_file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sqlite path has no file name"))
}

#[cfg(unix)]
fn secure_existing_sqlite_sidecars(
    sqlite_path: &Path,
    parent: &fs::File,
) -> Result<(), StateError> {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = sqlite_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        let file = match open_sqlite_file_nofollow(parent, &sidecar) {
            Ok(file) => file,
            Err(StateError::RestrictSqlitePermissions { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        secure_opened_sqlite_file(&sidecar, &file)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sqlite_file_open_error(path: &Path, source: io::Error) -> StateError {
    let is_symlink = source.raw_os_error() == Some(libc::ELOOP)
        || fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false);
    if is_symlink {
        StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::Symlink,
        }
    } else if source.kind() == io::ErrorKind::IsADirectory {
        StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::NonRegular,
        }
    } else {
        StateError::RestrictSqlitePermissions {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(unix)]
fn secure_opened_sqlite_file(path: &Path, file: &fs::File) -> Result<(), StateError> {
    // 中文注释：fstat 和 fchmod 都针对同一个 FD，路径在此期间被替换也不会触及外部目标。
    let metadata = file
        .metadata()
        .map_err(|source| StateError::RestrictSqlitePermissions {
            path: path.to_path_buf(),
            source,
        })?;
    let file_type = metadata.file_type();
    if !file_type.is_file() {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::NonRegular,
        });
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::WrongOwner,
        });
    }

    fchmod_opened_path(path, file, SQLITE_PRIVATE_FILE_MODE)
}

#[cfg(unix)]
fn fchmod_opened_path(path: &Path, file: &fs::File, mode: u32) -> Result<(), StateError> {
    // SAFETY: `file` 在调用期间保持存活，传入的 FD 仅供 fchmod 使用。
    let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(StateError::RestrictSqlitePermissions {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        })
    }
}

#[cfg(unix)]
fn verify_secure_sqlite_open(
    sqlite_path: &Path,
    secure_open: &SecureSqliteOpen,
) -> Result<(), StateError> {
    verify_path_identity(sqlite_path, &secure_open.database, false)?;
    verify_path_identity(&secure_open.parent_path, &secure_open.parent, true)
}

#[cfg(unix)]
fn verify_path_identity(path: &Path, opened: &fs::File, directory: bool) -> Result<(), StateError> {
    let expected = opened
        .metadata()
        .map_err(|source| sqlite_file_open_error(path, source))?;
    let actual =
        fs::symlink_metadata(path).map_err(|source| sqlite_file_open_error(path, source))?;
    if actual.file_type().is_symlink() {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::Symlink,
        });
    }
    let expected_type_matches = if directory {
        expected.is_dir() && actual.is_dir()
    } else {
        expected.is_file() && actual.is_file()
    };
    if !expected_type_matches || expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        return Err(StateError::UnsafeSqlitePath {
            path: path.to_path_buf(),
            kind: UnsafeSqlitePathKind::Replaced,
        });
    }
    Ok(())
}

fn ensure_compatible_connection(conn: &Connection, path: &Path) -> Result<(), StateError> {
    initialize_daemon_state_schema(conn, path)?;
    ensure_sqlite_state_version(conn, path)
}

fn ensure_state_version(path: &Path, found: u32) -> Result<(), StateError> {
    if found == STATE_SCHEMA_VERSION {
        return Ok(());
    }

    Err(StateError::IncompatibleVersion {
        path: path.to_path_buf(),
        found: Some(found),
        expected: STATE_SCHEMA_VERSION,
    })
}

fn ensure_sqlite_state_version(conn: &Connection, path: &Path) -> Result<(), StateError> {
    let schema_version = read_meta_value(conn, path, META_STATE_SCHEMA_VERSION)?;
    match schema_version {
        Some(raw) => {
            let found = raw.parse::<u32>().map_err(|source| StateError::Sqlite {
                path: path.to_path_buf(),
                source: rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source)),
            })?;
            if found == 2 {
                migrate_sqlite_v2_to_current(conn, path)?;
                return Ok(());
            }
            ensure_state_version(path, found)
        }
        None if sqlite_state_has_content(conn, path)? => Err(StateError::IncompatibleVersion {
            path: path.to_path_buf(),
            found: None,
            expected: STATE_SCHEMA_VERSION,
        }),
        None => Ok(()),
    }
}

fn migrate_legacy_state(path: &Path, mut state: DaemonState) -> Result<DaemonState, StateError> {
    match state.version {
        STATE_SCHEMA_VERSION => Ok(state),
        2 => {
            // 中文注释：v3 只调整 relay 信任边界，设备公钥认证格式没有变化；
            // 静默清空 trusted_devices 会让远程/headless daemon 失去所有控制端。
            state.version = STATE_SCHEMA_VERSION;
            Ok(state)
        }
        other => Err(StateError::IncompatibleVersion {
            path: path.to_path_buf(),
            found: Some(other),
            expected: STATE_SCHEMA_VERSION,
        }),
    }
}

fn migrate_sqlite_v2_to_current(conn: &Connection, path: &Path) -> Result<(), StateError> {
    write_sqlite_state_version(conn, path)
}

fn sqlite_state_has_content(conn: &Connection, path: &Path) -> Result<bool, StateError> {
    for (table, where_clause) in [
        (
            "daemon_meta",
            Some("key <> 'state_schema_version'".to_owned()),
        ),
        ("trusted_devices", None),
        ("runtime_sessions", None),
        ("http_uploads", None),
        ("daemon_clients", None),
        ("daemon_client_attached_sessions", None),
        ("daemon_sessions", None),
    ] {
        if sqlite_table_row_count(conn, path, table, where_clause.as_deref())? > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sqlite_table_row_count(
    conn: &Connection,
    path: &Path,
    table: &str,
    where_clause: Option<&str>,
) -> Result<u64, StateError> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))?
        .is_some();
    if !exists {
        return Ok(0);
    }

    let sql = match where_clause {
        Some(where_clause) => format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}"),
        None => format!("SELECT COUNT(*) FROM {table}"),
    };
    let count = conn
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(|source| sqlite_error(path, source))?;
    Ok(count.max(0) as u64)
}

fn write_sqlite_state_version(conn: &Connection, path: &Path) -> Result<(), StateError> {
    upsert_meta_value(
        conn,
        path,
        META_STATE_SCHEMA_VERSION,
        &STATE_SCHEMA_VERSION.to_string(),
        current_unix_timestamp_millis().0 as i64,
    )
}

fn initialize_daemon_state_schema(conn: &Connection, path: &Path) -> Result<(), StateError> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS daemon_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS trusted_devices (
            device_id TEXT PRIMARY KEY,
            public_key TEXT NOT NULL,
            trusted_at_ms INTEGER NOT NULL,
            last_seen_at_ms INTEGER,
            label TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_trusted_devices_seen
            ON trusted_devices(last_seen_at_ms, device_id);

        CREATE TABLE IF NOT EXISTS runtime_sessions (
            session_id TEXT PRIMARY KEY,
            state TEXT NOT NULL,
            rows INTEGER NOT NULL,
            cols INTEGER NOT NULL,
            pixel_width INTEGER NOT NULL,
            pixel_height INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            restore_kind TEXT,
            restore_value TEXT
        );

        CREATE TABLE IF NOT EXISTS http_uploads (
            upload_id TEXT PRIMARY KEY,
            target_path TEXT NOT NULL,
            size_bytes TEXT NOT NULL,
            dev TEXT NOT NULL,
            ino TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        "#,
    )
    .map_err(|source| sqlite_error(path, source))
}

fn list_http_uploads(
    conn: &Connection,
    path: &Path,
) -> Result<Vec<HttpUploadRecoveryRecord>, StateError> {
    let mut stmt = conn
        .prepare(
            r#"
            SELECT upload_id, target_path, size_bytes, dev, ino, updated_at_ms
            FROM http_uploads
            ORDER BY updated_at_ms, upload_id
            "#,
        )
        .map_err(|source| sqlite_error(path, source))?;
    let rows = stmt
        .query_map([], |row| {
            let size_raw = row.get::<_, String>(2)?;
            let dev_raw = row.get::<_, String>(3)?;
            let ino_raw = row.get::<_, String>(4)?;
            Ok(HttpUploadRecoveryRecord {
                upload_id: row.get::<_, String>(0)?,
                target_path: PathBuf::from(row.get::<_, String>(1)?),
                size_bytes: size_raw.parse::<u64>().map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(source))
                })?,
                dev: dev_raw.parse::<u64>().map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(source))
                })?,
                ino: ino_raw.parse::<u64>().map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(4, Type::Text, Box::new(source))
                })?,
                updated_at_ms: integer_to_timestamp(row.get::<_, i64>(5)?, 5)?,
            })
        })
        .map_err(|source| sqlite_error(path, source))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|source| sqlite_error(path, source))?);
    }
    Ok(records)
}

fn load_sqlite_state(conn: &Connection, path: &Path) -> Result<DaemonState, StateError> {
    let server_id = read_meta_value(conn, path, META_SERVER_ID)?;
    let public_key = read_meta_value(conn, path, META_DAEMON_PUBLIC_KEY)?;
    let private_key = read_meta_value(conn, path, META_DAEMON_PRIVATE_KEY)?;
    let daemon_identity = match (server_id, public_key) {
        (Some(server_id), Some(public_key)) => Some(DaemonIdentitySnapshot {
            server_id: parse_server_id(path, server_id)?,
            public_key: PublicKey(public_key),
            private_key,
        }),
        _ => None,
    };

    let mut trusted_devices = Vec::new();
    let mut stmt = conn
        .prepare(
            r#"
            SELECT device_id, public_key, trusted_at_ms, last_seen_at_ms, label
            FROM trusted_devices
            ORDER BY device_id
            "#,
        )
        .map_err(|source| sqlite_error(path, source))?;
    let rows = stmt
        .query_map([], |row| {
            let device_id = parse_device_id(row.get::<_, String>(0)?)?;
            let public_key = PublicKey(row.get::<_, String>(1)?);
            let trusted_at_ms = integer_to_timestamp(row.get::<_, i64>(2)?, 2)?;
            let last_seen_at_ms = row
                .get::<_, Option<i64>>(3)?
                .map(|value| integer_to_timestamp(value, 3))
                .transpose()?;
            let label = row.get::<_, Option<String>>(4)?;

            Ok(TrustedDeviceState {
                device_id,
                public_key,
                trusted_at_ms,
                last_seen_at_ms,
                label,
            })
        })
        .map_err(|source| sqlite_error(path, source))?;

    for row in rows {
        trusted_devices.push(row.map_err(|source| sqlite_error(path, source))?);
    }

    let mut sessions = Vec::new();
    let mut stmt = conn
        .prepare(
            r#"
            SELECT
                session_id,
                state,
                rows,
                cols,
                pixel_width,
                pixel_height,
                created_at_ms,
                updated_at_ms,
                restore_kind,
                restore_value
            FROM runtime_sessions
            ORDER BY created_at_ms, session_id
            "#,
        )
        .map_err(|source| sqlite_error(path, source))?;
    let rows = stmt
        .query_map([], |row| {
            let raw_session_id = row.get::<_, String>(0)?;
            let session_id = Uuid::parse_str(&raw_session_id)
                .map(SessionId)
                .map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source))
                })?;
            let state = parse_session_state(row.get::<_, String>(1)?)?;
            let size = TerminalSize {
                rows: integer_to_u16(row.get::<_, i64>(2)?, 2)?,
                cols: integer_to_u16(row.get::<_, i64>(3)?, 3)?,
                pixel_width: integer_to_u16(row.get::<_, i64>(4)?, 4)?,
                pixel_height: integer_to_u16(row.get::<_, i64>(5)?, 5)?,
            };
            let restore_kind = row.get::<_, Option<String>>(8)?;
            let restore_value = row.get::<_, Option<String>>(9)?;
            let restore_info = parse_restore_info(restore_kind, restore_value)?;
            Ok(SessionStateRecord {
                session_id,
                state,
                size,
                created_at_ms: integer_to_timestamp(row.get::<_, i64>(6)?, 6)?,
                updated_at_ms: integer_to_timestamp(row.get::<_, i64>(7)?, 7)?,
                restore_info,
            })
        })
        .map_err(|source| sqlite_error(path, source))?;

    for row in rows {
        sessions.push(row.map_err(|source| sqlite_error(path, source))?);
    }

    Ok(DaemonState {
        version: STATE_SCHEMA_VERSION,
        daemon_identity,
        trusted_devices,
        sessions,
    })
}

fn read_meta_value(
    conn: &Connection,
    path: &Path,
    key: &'static str,
) -> Result<Option<String>, StateError> {
    conn.query_row(
        "SELECT value FROM daemon_meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|source| sqlite_error(path, source))
}

fn save_sqlite_state(
    conn: &mut Connection,
    path: &Path,
    state: &DaemonState,
) -> Result<(), StateError> {
    ensure_state_version(path, state.version)?;

    let now_ms = current_unix_timestamp_millis().0 as i64;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| sqlite_error(path, source))?;

    // 中文注释：这里显式写 schema version，避免旧阶段遗留的 restore state 被当前
    // supervisor-only runtime 静默复用。
    upsert_meta_value(
        &tx,
        path,
        META_STATE_SCHEMA_VERSION,
        &state.version.to_string(),
        now_ms,
    )?;

    match &state.daemon_identity {
        Some(identity) => {
            upsert_meta_value(
                &tx,
                path,
                META_SERVER_ID,
                &identity.server_id.0.to_string(),
                now_ms,
            )?;
            upsert_meta_value(
                &tx,
                path,
                META_DAEMON_PUBLIC_KEY,
                &identity.public_key.0,
                now_ms,
            )?;
            if let Some(private_key) = identity.private_key.as_deref() {
                upsert_meta_value(&tx, path, META_DAEMON_PRIVATE_KEY, private_key, now_ms)?;
            } else {
                tx.execute(
                    "DELETE FROM daemon_meta WHERE key = ?1",
                    params![META_DAEMON_PRIVATE_KEY],
                )
                .map_err(|source| sqlite_error(path, source))?;
            }
        }
        None => {
            tx.execute(
                "DELETE FROM daemon_meta WHERE key IN (?1, ?2, ?3)",
                params![
                    META_SERVER_ID,
                    META_DAEMON_PUBLIC_KEY,
                    META_DAEMON_PRIVATE_KEY
                ],
            )
            .map_err(|source| sqlite_error(path, source))?;
        }
    }

    tx.execute("DELETE FROM trusted_devices", [])
        .map_err(|source| sqlite_error(path, source))?;
    for device in &state.trusted_devices {
        tx.execute(
            r#"
            INSERT INTO trusted_devices (
                device_id,
                public_key,
                trusted_at_ms,
                last_seen_at_ms,
                label
            )
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                device.device_id.0.to_string(),
                device.public_key.0.as_str(),
                device.trusted_at_ms.0 as i64,
                device.last_seen_at_ms.map(|timestamp| timestamp.0 as i64),
                device.label.as_deref(),
            ],
        )
        .map_err(|source| sqlite_error(path, source))?;
    }

    // 这里不能把“本次快照里缺失”的旧 runtime 行自动标成 closed。快照生成依赖 runtime
    // 状态、尺寸和 supervisor restore_info，任一临时读取失败都可能漏掉仍存活的 session。
    // 显式关闭或恢复失败必须调用 `record_runtime_session_closed` 写 tombstone。

    for session in &state.sessions {
        let (restore_kind, restore_value) = if session.state == SessionState::Closed {
            (None, None)
        } else {
            serialize_restore_info(session.restore_info.as_ref())
        };
        tx.execute(
            r#"
            INSERT INTO runtime_sessions (
                session_id,
                state,
                rows,
                cols,
                pixel_width,
                pixel_height,
                created_at_ms,
                updated_at_ms,
                restore_kind,
                restore_value
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(session_id) DO UPDATE SET
                state = CASE
                    WHEN runtime_sessions.state = ?11 OR excluded.state = ?11 THEN ?11
                    ELSE excluded.state
                END,
                rows = excluded.rows,
                cols = excluded.cols,
                pixel_width = excluded.pixel_width,
                pixel_height = excluded.pixel_height,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms,
                restore_kind = CASE
                    WHEN runtime_sessions.state = ?11 OR excluded.state = ?11 THEN NULL
                    ELSE excluded.restore_kind
                END,
                restore_value = CASE
                    WHEN runtime_sessions.state = ?11 OR excluded.state = ?11 THEN NULL
                    ELSE excluded.restore_value
                END
            "#,
            params![
                session.session_id.0.to_string(),
                session_state_text(session.state),
                i64::from(session.size.rows),
                i64::from(session.size.cols),
                i64::from(session.size.pixel_width),
                i64::from(session.size.pixel_height),
                session.created_at_ms.0 as i64,
                session.updated_at_ms.0 as i64,
                restore_kind,
                restore_value,
                session_state_text(SessionState::Closed),
            ],
        )
        .map_err(|source| sqlite_error(path, source))?;
    }

    tx.commit().map_err(|source| sqlite_error(path, source))
}

fn prune_closed_runtime_sessions_except(
    conn: &mut Connection,
    path: &Path,
    protected_session_ids: &HashSet<SessionId>,
) -> Result<usize, StateError> {
    let protected = protected_session_ids
        .iter()
        .map(|session_id| session_id.0.to_string())
        .collect::<HashSet<_>>();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| sqlite_error(path, source))?;
    let mut stmt = tx
        .prepare(
            r#"
            SELECT session_id
            FROM runtime_sessions
            WHERE state = ?1
            "#,
        )
        .map_err(|source| sqlite_error(path, source))?;
    let rows = stmt
        .query_map(params![session_state_text(SessionState::Closed)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| sqlite_error(path, source))?;
    let mut deletable = Vec::new();
    for row in rows {
        let session_id = row.map_err(|source| sqlite_error(path, source))?;
        if !protected.contains(&session_id) {
            deletable.push(session_id);
        }
    }
    drop(stmt);

    let mut deleted = 0;
    for session_id in deletable {
        // closed 且没有 live supervisor 保护的 runtime 行已经不可恢复，保留只会污染统计。
        deleted += tx
            .execute(
                "DELETE FROM runtime_sessions WHERE session_id = ?1 AND state = ?2",
                params![session_id, session_state_text(SessionState::Closed)],
            )
            .map_err(|source| sqlite_error(path, source))?;
    }
    tx.commit().map_err(|source| sqlite_error(path, source))?;
    Ok(deleted)
}

fn upsert_meta_value(
    conn: &Connection,
    path: &Path,
    key: &'static str,
    value: &str,
    now_ms: i64,
) -> Result<(), StateError> {
    conn.execute(
        r#"
        INSERT INTO daemon_meta (key, value, updated_at_ms)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at_ms = excluded.updated_at_ms
        "#,
        params![key, value, now_ms],
    )
    .map_err(|source| sqlite_error(path, source))?;
    Ok(())
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> StateError {
    StateError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn parse_server_id(path: &Path, raw: String) -> Result<ServerId, StateError> {
    Uuid::parse_str(&raw)
        .map(ServerId)
        .map_err(|source| sqlite_error_from_conversion(path, source))
}

fn parse_device_id(raw: String) -> rusqlite::Result<DeviceId> {
    Uuid::parse_str(&raw).map(DeviceId).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source))
    })
}

fn parse_session_state(raw: String) -> rusqlite::Result<SessionState> {
    match raw.as_str() {
        "created" => Ok(SessionState::Created),
        "running" => Ok(SessionState::Running),
        "closed" => Ok(SessionState::Closed),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            1,
            Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid runtime session state `{other}`"),
            )),
        )),
    }
}

fn integer_to_u16(value: i64, column: usize) -> rusqlite::Result<u16> {
    u16::try_from(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(source))
    })
}

fn integer_to_timestamp(value: i64, column: usize) -> rusqlite::Result<UnixTimestampMillis> {
    u64::try_from(value)
        .map(UnixTimestampMillis)
        .map_err(|source| {
            rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(source))
        })
}

fn session_state_text(state: SessionState) -> &'static str {
    match state {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::Closed => "closed",
    }
}

pub(crate) fn serialize_restore_info(
    restore_info: Option<&PtyRestoreInfo>,
) -> (Option<&'static str>, Option<String>) {
    match restore_info {
        Some(PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            supervisor_status,
        }) => {
            let value = SerializedUnixSocketRestoreInfo {
                socket_path: socket_path.clone(),
                supervisor_pid: *supervisor_pid,
                supervisor_status: *supervisor_status,
            };
            (
                Some("unix_socket"),
                Some(
                    serde_json::to_string(&value)
                        .expect("supervisor restore info should always serialize"),
                ),
            )
        }
        Some(PtyRestoreInfo::Tmux {
            socket_path,
            session_name,
        }) => {
            let value = SerializedTmuxRestoreInfo {
                socket_path: socket_path.clone(),
                session_name: session_name.clone(),
            };
            (
                Some("tmux"),
                Some(
                    serde_json::to_string(&value)
                        .expect("tmux restore info should always serialize"),
                ),
            )
        }
        None => (None, None),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedUnixSocketRestoreInfo {
    socket_path: PathBuf,
    supervisor_pid: u32,
    supervisor_status: PtySupervisorStatus,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedTmuxRestoreInfo {
    socket_path: PathBuf,
    session_name: String,
}

fn parse_restore_info(
    restore_kind: Option<String>,
    restore_value: Option<String>,
) -> rusqlite::Result<Option<PtyRestoreInfo>> {
    match (restore_kind.as_deref(), restore_value) {
        (None, None) => Ok(None),
        (Some("unix_socket"), Some(raw_value)) => {
            let value: SerializedUnixSocketRestoreInfo =
                serde_json::from_str(&raw_value).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(9, Type::Text, Box::new(source))
                })?;
            Ok(Some(PtyRestoreInfo::UnixSocket {
                socket_path: value.socket_path,
                supervisor_pid: value.supervisor_pid,
                supervisor_status: value.supervisor_status,
            }))
        }
        (Some("tmux"), Some(raw_value)) => {
            let value: SerializedTmuxRestoreInfo =
                serde_json::from_str(&raw_value).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(9, Type::Text, Box::new(source))
                })?;
            Ok(Some(PtyRestoreInfo::Tmux {
                socket_path: value.socket_path,
                session_name: value.session_name,
            }))
        }
        (kind, value) => Err(rusqlite::Error::FromSqlConversionFailure(
            8,
            Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid runtime session restore info kind={kind:?} value={value:?}"),
            )),
        )),
    }
}

fn sqlite_error_from_conversion(path: &Path, source: uuid::Error) -> StateError {
    StateError::Sqlite {
        path: path.to_path_buf(),
        source: rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source)),
    }
}

fn current_unix_timestamp_millis() -> UnixTimestampMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    UnixTimestampMillis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use termd_proto::{
        DeviceId, PublicKey, ServerId, SessionId, SessionState, TerminalSize, UnixTimestampMillis,
    };

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "termd-state-test-{}-{}-{name}",
            std::process::id(),
            nanos
        ));
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory.join(name)
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
    fn state_store_saves_and_loads_sqlite_daemon_state_without_runtime_sessions() {
        let state_path = temp_path("daemon-state.json");
        let state = sample_state();

        StateStore::save(&state_path, &state).unwrap();

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded.version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.daemon_identity, state.daemon_identity);
        assert_eq!(loaded.trusted_devices, state.trusted_devices);
        assert_eq!(loaded.sessions, state.sessions);
        assert!(!state_path.exists());
        assert!(sqlite_state_path_for_state_path(&state_path).exists());
        cleanup_state_paths(&state_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_creates_sqlite_database_with_private_permissions() {
        let state_path = temp_path("private-new-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);

        let loaded = StateStore::load(&state_path).unwrap();

        assert_eq!(loaded, DaemonState::default());
        assert_eq!(unix_mode(&sqlite_path), 0o600);
        cleanup_state_paths(&state_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_accepts_readable_existing_parent_directories_without_chmod() {
        for mode in [0o750, 0o755] {
            let state_path = temp_path(&format!("readable-parent-{mode:o}.json"));
            let parent = state_path.parent().unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(mode)).unwrap();

            StateStore::load(&state_path).unwrap();

            assert_eq!(unix_mode(parent), mode);
            cleanup_state_paths(&state_path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_writable_existing_parent_directories_without_chmod() {
        for mode in [0o770, 0o702] {
            let state_path = temp_path(&format!("writable-parent-{mode:o}.json"));
            let parent = state_path.parent().unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(mode)).unwrap();

            let error = StateStore::load(&state_path).unwrap_err();

            assert!(matches!(error, StateError::UnsafeSqlitePath { .. }));
            assert_eq!(unix_mode(parent), mode);
            assert!(!sqlite_state_path_for_state_path(&state_path).exists());
            let _ = fs::remove_dir(parent);
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_store_allows_sticky_world_writable_ancestor() {
        let root_state_path = temp_path("sticky-ancestor-root");
        let root = root_state_path.parent().unwrap().to_path_buf();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o1777)).unwrap();
        let private_parent = root.join("private");
        fs::create_dir(&private_parent).unwrap();
        fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = private_parent.join("daemon-state.json");

        StateStore::load(&state_path).unwrap();

        cleanup_state_paths(&state_path);
        let _ = fs::remove_dir(root);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_non_sticky_world_writable_ancestor() {
        let root_state_path = temp_path("writable-ancestor-root");
        let root = root_state_path.parent().unwrap().to_path_buf();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        let private_parent = root.join("private");
        fs::create_dir(&private_parent).unwrap();
        fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = private_parent.join("daemon-state.json");

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(error, StateError::UnsafeSqlitePath { .. }));
        assert_eq!(unix_mode(&root), 0o777);
        assert!(!sqlite_state_path_for_state_path(&state_path).exists());
        let _ = fs::remove_dir(private_parent);
        let _ = fs::remove_dir(root);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_foreign_owned_ancestor_regardless_of_mode() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        for mode in [0o555, 0o1555] {
            let root_state_path = temp_path(&format!("foreign-ancestor-{mode:o}-root"));
            let root = root_state_path.parent().unwrap().to_path_buf();
            let foreign_ancestor = root.join("foreign");
            let private_parent = foreign_ancestor.join("private");
            fs::create_dir(&foreign_ancestor).unwrap();
            fs::create_dir(&private_parent).unwrap();
            fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
            chown_path(&foreign_ancestor, 65_534, 65_534);
            fs::set_permissions(&foreign_ancestor, fs::Permissions::from_mode(mode)).unwrap();
            let state_path = private_parent.join("daemon-state.json");

            let error = StateStore::load(&state_path).unwrap_err();

            assert!(matches!(error, StateError::UnsafeSqlitePath { .. }));
            assert!(!sqlite_state_path_for_state_path(&state_path).exists());
            chown_path(&foreign_ancestor, unsafe { libc::geteuid() }, unsafe {
                libc::getegid()
            });
            fs::set_permissions(&foreign_ancestor, fs::Permissions::from_mode(0o700)).unwrap();
            let _ = fs::remove_dir(private_parent);
            let _ = fs::remove_dir(foreign_ancestor);
            let _ = fs::remove_dir(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_store_securely_creates_multiple_missing_parent_components() {
        let root_state_path = temp_path("multi-level-missing-root");
        let root = root_state_path.parent().unwrap().to_path_buf();
        let first = root.join("missing");
        let second = first.join("a");
        let parent = second.join("state");
        let state_path = parent.join("daemon-state.json");

        let loaded = StateStore::load(&state_path).unwrap();

        assert_eq!(loaded, DaemonState::default());
        for directory in [&first, &second, &parent] {
            assert_eq!(unix_mode(directory), 0o700);
        }
        cleanup_state_paths(&state_path);
        let _ = fs::remove_dir(second);
        let _ = fs::remove_dir(first);
        let _ = fs::remove_dir(root);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_anchors_relative_path_before_cwd_changes() {
        const TEST_NAME: &str =
            "state::tests::state_store_anchors_relative_path_before_cwd_changes";
        const CHILD_CWD_ENV: &str = "TERMD_TEST_CWD_AFTER_SQLITE_PREPARE";

        if std::env::var_os(CHILD_CWD_ENV).is_some() {
            let loaded = StateStore::load("state/../state/daemon-state.json").unwrap();
            assert_eq!(
                loaded.trusted_devices[0].label.as_deref(),
                Some("initial-cwd")
            );
            return;
        }

        let root_state_path = temp_path("relative-cwd-anchor-root");
        let root = root_state_path.parent().unwrap().to_path_buf();
        let state_parent = root.join("state");
        let alternate_cwd = root.join("alternate");
        fs::create_dir(&state_parent).unwrap();
        fs::create_dir(&alternate_cwd).unwrap();
        fs::create_dir(alternate_cwd.join("state")).unwrap();
        for directory in [&state_parent, &alternate_cwd, &alternate_cwd.join("state")] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let expected_state_path = state_parent.join("daemon-state.json");
        let alternate_state_path = alternate_cwd.join("state/daemon-state.json");
        let mut initial_state = sample_state();
        initial_state.trusted_devices[0].label = Some("initial-cwd".to_owned());
        let mut alternate_state = sample_state();
        alternate_state.trusted_devices[0].label = Some("alternate-cwd".to_owned());
        fs::write(
            &expected_state_path,
            serde_json::to_string_pretty(&initial_state).unwrap(),
        )
        .unwrap();
        fs::write(
            &alternate_state_path,
            serde_json::to_string_pretty(&alternate_state).unwrap(),
        )
        .unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .current_dir(&root)
            .env(CHILD_CWD_ENV, &alternate_cwd)
            .output()
            .unwrap();
        let unexpected_sqlite_path = alternate_cwd.join("state/daemon-state.sqlite");
        let expected_sqlite_exists =
            sqlite_state_path_for_state_path(&expected_state_path).exists();
        let unexpected_sqlite_exists = unexpected_sqlite_path.exists();

        cleanup_state_paths(&expected_state_path);
        cleanup_state_paths(&alternate_state_path);
        let _ = fs::remove_dir(&alternate_cwd);
        let _ = fs::remove_dir(&root);

        assert!(
            output.status.success(),
            "child test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(expected_sqlite_exists);
        assert!(!unexpected_sqlite_exists);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_symlink_ancestor_directory() {
        let target_state_path = temp_path("ancestor-target");
        let target_root = target_state_path.parent().unwrap().to_path_buf();
        let private_parent = target_root.join("private");
        fs::create_dir(&private_parent).unwrap();
        fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let link_state_path = temp_path("ancestor-link-root");
        let link_root = link_state_path.parent().unwrap().to_path_buf();
        let linked_ancestor = link_root.join("linked");
        symlink(&target_root, &linked_ancestor).unwrap();
        let state_path = linked_ancestor.join("private/daemon-state.json");

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(error, StateError::UnsafeSqlitePath { .. }));
        assert!(!private_parent.join("daemon-state.sqlite").exists());
        let _ = fs::remove_file(linked_ancestor);
        let _ = fs::remove_dir(link_root);
        let _ = fs::remove_dir(private_parent);
        let _ = fs::remove_dir(target_root);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_symlink_parent_directory() {
        let target_state_path = temp_path("parent-target-state.json");
        let target_parent = target_state_path.parent().unwrap().to_path_buf();
        let link_root = temp_path("parent-link-root");
        let link_root_parent = link_root.parent().unwrap().to_path_buf();
        let linked_parent = link_root_parent.join("linked-parent");
        symlink(&target_parent, &linked_parent).unwrap();
        let state_path = linked_parent.join("daemon-state.json");

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(error, StateError::UnsafeSqlitePath { .. }));
        assert!(!target_parent.join("daemon-state.sqlite").exists());
        let _ = fs::remove_file(linked_parent);
        let _ = fs::remove_dir(link_root_parent);
        let _ = fs::remove_dir(target_parent);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_database_and_generated_sidecars_have_private_permissions() {
        let state_path = temp_path("private-existing-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let sidecars = sqlite_sidecar_paths(&sqlite_path);
        let state = sample_state();

        StateStore::save(&state_path, &state).unwrap();
        let keeper = force_wal_sidecars(&sqlite_path);

        assert_eq!(unix_mode(&sqlite_path), 0o600);
        assert_eq!(unix_mode(&sidecars[0]), 0o600);
        assert_eq!(unix_mode(&sidecars[1]), 0o600);
        assert_eq!(unix_mode(state_path.parent().unwrap()), 0o700);
        drop(keeper);
        cleanup_state_paths(&state_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_restricts_existing_nonempty_sidecars_before_reopen() {
        let state_path = temp_path("existing-sidecar-permissions.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let sidecars = sqlite_sidecar_paths(&sqlite_path);
        StateStore::save(&state_path, &sample_state()).unwrap();
        let keeper = force_wal_sidecars(&sqlite_path);
        for sidecar in &sidecars {
            assert!(fs::metadata(sidecar).unwrap().len() > 0);
            set_unix_mode(sidecar, 0o644);
        }

        StateStore::load(&state_path).unwrap();

        assert_eq!(unix_mode(&sidecars[0]), 0o600);
        assert_eq!(unix_mode(&sidecars[1]), 0o600);
        drop(keeper);
        cleanup_state_paths(&state_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_sqlite_symlink_without_touching_target() {
        let state_path = temp_path("symlink-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let target_path = temp_path("symlink-target.sqlite");

        fs::write(&target_path, "").unwrap();
        set_unix_mode(&target_path, 0o644);
        symlink(&target_path, &sqlite_path).unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert_unsafe_sqlite_path(error, &sqlite_path, UnsafeSqlitePathKind::Symlink);
        assert_eq!(unix_mode(&target_path), 0o644);
        cleanup_state_paths(&state_path);
        cleanup_temp_file(&target_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_non_regular_sqlite_path() {
        let state_path = temp_path("directory-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);

        fs::create_dir_all(&sqlite_path).unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert_unsafe_sqlite_path(error, &sqlite_path, UnsafeSqlitePathKind::NonRegular);
        let _ = fs::remove_dir(&sqlite_path);
        cleanup_state_paths(&state_path);
    }

    #[cfg(unix)]
    #[test]
    fn state_store_rejects_sqlite_sidecar_symlink_without_touching_target() {
        let state_path = temp_path("sidecar-symlink-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let sidecars = sqlite_sidecar_paths(&sqlite_path);
        let target_path = temp_path("sidecar-target.sqlite-wal");

        StateStore::save(&state_path, &sample_state()).unwrap();
        for sidecar_path in &sidecars {
            let _ = fs::remove_file(sidecar_path);
        }
        fs::write(&target_path, "").unwrap();
        set_unix_mode(&target_path, 0o644);
        symlink(&target_path, &sidecars[0]).unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert_unsafe_sqlite_path(error, &sidecars[0], UnsafeSqlitePathKind::Symlink);
        assert_eq!(unix_mode(&target_path), 0o644);
        cleanup_state_paths(&state_path);
        cleanup_temp_file(&target_path);
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_file_permission_and_identity_checks_use_opened_fd() {
        let state_path = temp_path("fd-permission-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let detached_path = state_path.with_file_name("fd-permission-detached.sqlite");
        let target_path = state_path.with_file_name("fd-permission-target.sqlite");
        fs::write(&sqlite_path, "").unwrap();
        fs::write(&target_path, "").unwrap();
        set_unix_mode(&sqlite_path, 0o644);
        set_unix_mode(&target_path, 0o644);

        let parent = open_directory_nofollow(sqlite_path.parent().unwrap()).unwrap();
        let file = open_sqlite_file_nofollow(&parent, &sqlite_path).unwrap();
        fs::rename(&sqlite_path, &detached_path).unwrap();
        symlink(&target_path, &sqlite_path).unwrap();

        secure_opened_sqlite_file(&sqlite_path, &file).unwrap();

        assert_eq!(unix_mode(&detached_path), 0o600);
        assert_eq!(unix_mode(&target_path), 0o644);
        assert!(matches!(
            verify_path_identity(&sqlite_path, &file, false),
            Err(StateError::UnsafeSqlitePath {
                kind: UnsafeSqlitePathKind::Symlink,
                ..
            })
        ));
        drop(file);
        let _ = fs::remove_file(detached_path);
        let _ = fs::remove_file(target_path);

        cleanup_state_paths(&state_path);
    }

    #[test]
    fn sqlite_open_flags_do_not_enable_create_or_uri_parsing() {
        // 中文注释：路径安全检查按普通 Path 完成，SQLite 打开层不能重新启用 URI 语义。
        assert!(
            !sqlite_open_flags().contains(OpenFlags::SQLITE_OPEN_URI),
            "SQLite 状态库打开不能包含 SQLITE_OPEN_URI"
        );
        assert!(
            !sqlite_open_flags().contains(OpenFlags::SQLITE_OPEN_CREATE),
            "SQLite 状态库必须先安全创建，再以不含 SQLITE_OPEN_CREATE 的 flags 打开"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn state_store_rejects_unsupported_platform_without_creating_paths() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = std::env::temp_dir()
            .join(format!("termd-unsupported-{}-{nanos}", std::process::id()))
            .join("missing/daemon-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(
            error,
            StateError::UnsupportedPlatform { path } if path == sqlite_path
        ));
        assert!(!state_path.parent().unwrap().exists());
    }

    #[test]
    fn missing_state_load_returns_empty_default_state() {
        let state_path = temp_path("missing-state.json");

        let loaded = StateStore::load(&state_path).unwrap();

        assert_eq!(loaded, DaemonState::default());
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn corrupted_state_json_returns_structured_error() {
        let state_path = temp_path("corrupted-state.json");
        fs::write(&state_path, "{not-json").unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(error, StateError::Parse { .. }));
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn state_save_does_not_rewrite_legacy_json_target() {
        let state_path = temp_path("sqlite-state.json");
        let state = sample_state();

        StateStore::save(&state_path, &state).unwrap();

        assert!(!state_path.exists());
        assert!(sqlite_state_path_for_state_path(&state_path).exists());
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn state_load_rejects_oversized_terminal_size_from_sqlite() {
        let state_path = temp_path("oversized-runtime-session-size.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        StateStore::save(&state_path, &sample_state()).unwrap();
        let conn = Connection::open(&sqlite_path).unwrap();
        conn.execute(
            "UPDATE runtime_sessions SET cols = ?1",
            params![i64::from(u16::MAX) + 1],
        )
        .unwrap();
        drop(conn);

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(
            error,
            StateError::Sqlite {
                source: rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Integer,
                    _
                ),
                ..
            }
        ));
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn state_save_preserves_missing_runtime_sessions_until_explicit_close() {
        let state_path = temp_path("closed-runtime-session.json");
        let running_state = sample_state();
        let session_id = running_state.sessions[0].session_id;
        let mut empty_live_snapshot = running_state.clone();
        empty_live_snapshot.sessions.clear();

        StateStore::save(&state_path, &running_state).unwrap();
        StateStore::save(&state_path, &empty_live_snapshot).unwrap();

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].session_id, session_id);
        assert_eq!(loaded.sessions[0].state, SessionState::Running);
        assert!(loaded.sessions[0].restore_info.is_some());

        let updated = StateStore::record_runtime_session_closed(
            &state_path,
            session_id,
            UnixTimestampMillis(3_000),
        )
        .unwrap();
        assert!(updated);

        let closed = StateStore::load(&state_path).unwrap();
        assert_eq!(closed.sessions.len(), 1);
        assert_eq!(closed.sessions[0].session_id, session_id);
        assert_eq!(closed.sessions[0].state, SessionState::Closed);
        assert!(closed.sessions[0].restore_info.is_none());
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn closed_tombstone_upserts_when_runtime_row_was_never_persisted() {
        let state_path = temp_path("closed-runtime-session-missing-row.json");
        let session_id = SessionId::new();
        StateStore::save(&state_path, &DaemonState::default()).unwrap();

        assert!(
            StateStore::record_runtime_session_closed(
                &state_path,
                session_id,
                UnixTimestampMillis(3_000),
            )
            .unwrap()
        );

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].session_id, session_id);
        assert_eq!(loaded.sessions[0].state, SessionState::Closed);
        assert!(loaded.sessions[0].restore_info.is_none());
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn stale_running_snapshot_cannot_reopen_closed_runtime_session() {
        let state_path = temp_path("closed-runtime-session-stale-snapshot.json");
        let running_state = sample_state();
        let session_id = running_state.sessions[0].session_id;

        StateStore::save(&state_path, &running_state).unwrap();
        assert!(
            StateStore::record_runtime_session_closed(
                &state_path,
                session_id,
                UnixTimestampMillis(3_000),
            )
            .unwrap()
        );

        let mut stale_snapshot = running_state;
        stale_snapshot.sessions[0].size = TerminalSize::new(40, 120);
        stale_snapshot.sessions[0].updated_at_ms = UnixTimestampMillis(4_000);
        StateStore::save(&state_path, &stale_snapshot).unwrap();

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].session_id, session_id);
        assert_eq!(loaded.sessions[0].state, SessionState::Closed);
        assert_eq!(loaded.sessions[0].size, TerminalSize::new(40, 120));
        assert!(loaded.sessions[0].restore_info.is_none());
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn state_prune_closed_sessions_removes_only_unprotected_rows() {
        let state_path = temp_path("prune-closed-runtime-session.json");
        let running_state = sample_state();
        let protected_session_id = running_state.sessions[0].session_id;
        let mut closed_state = running_state.clone();
        closed_state.sessions.push(SessionStateRecord {
            session_id: SessionId::new(),
            state: SessionState::Running,
            size: TerminalSize::new(30, 100),
            created_at_ms: UnixTimestampMillis(1_500),
            updated_at_ms: UnixTimestampMillis(1_500),
            restore_info: Some(PtyRestoreInfo::UnixSocket {
                socket_path: PathBuf::from("/tmp/protected.sock"),
                supervisor_pid: 99,
                supervisor_status: PtySupervisorStatus::Running,
            }),
        });
        closed_state.sessions.push(SessionStateRecord {
            session_id: SessionId::new(),
            state: SessionState::Closed,
            size: TerminalSize::new(24, 80),
            created_at_ms: UnixTimestampMillis(2_000),
            updated_at_ms: UnixTimestampMillis(2_000),
            restore_info: None,
        });

        StateStore::save(&state_path, &closed_state).unwrap();
        let protected = [protected_session_id].into_iter().collect::<HashSet<_>>();
        let deleted = StateStore::prune_closed_sessions_except(&state_path, &protected).unwrap();
        assert_eq!(deleted, 1);

        let loaded = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded.sessions.len(), 2);
        assert!(
            loaded
                .sessions
                .iter()
                .all(|session| session.state == SessionState::Running)
        );
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn legacy_json_state_is_read_once_and_then_sqlite_wins() {
        let state_path = temp_path("legacy-state.json");
        let legacy_state = sample_state();
        let mut sqlite_state = legacy_state.clone();
        sqlite_state.trusted_devices[0].label = Some("sqlite-wins".to_owned());

        fs::write(
            &state_path,
            serde_json::to_string_pretty(&legacy_state).unwrap(),
        )
        .unwrap();

        let loaded_from_legacy = StateStore::load(&state_path).unwrap();
        assert_eq!(loaded_from_legacy, legacy_state);

        StateStore::save(&state_path, &sqlite_state).unwrap();
        fs::write(&state_path, "{not-json").unwrap();

        let loaded_from_sqlite = StateStore::load(&state_path).unwrap();
        assert_eq!(
            loaded_from_sqlite.daemon_identity,
            sqlite_state.daemon_identity
        );
        assert_eq!(
            loaded_from_sqlite.trusted_devices,
            sqlite_state.trusted_devices
        );
        assert_eq!(loaded_from_sqlite.sessions, sqlite_state.sessions);
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn old_legacy_json_state_is_rejected_after_supervisor_schema_bump() {
        let state_path = temp_path("old-legacy-state.json");
        let mut legacy_state = sample_state();
        legacy_state.version = 1;

        fs::write(
            &state_path,
            serde_json::to_string_pretty(&legacy_state).unwrap(),
        )
        .unwrap();

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(
            error,
            StateError::IncompatibleVersion {
                found: Some(1),
                expected: STATE_SCHEMA_VERSION,
                ..
            }
        ));
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn v2_legacy_json_migration_keeps_trusted_devices_and_sessions() {
        let state_path = temp_path("v2-legacy-state.json");
        let mut legacy_state = sample_state();
        legacy_state.version = 2;

        fs::write(
            &state_path,
            serde_json::to_string_pretty(&legacy_state).unwrap(),
        )
        .unwrap();

        let migrated = StateStore::load(&state_path).unwrap();

        assert_eq!(migrated.version, STATE_SCHEMA_VERSION);
        assert_eq!(migrated.daemon_identity, legacy_state.daemon_identity);
        assert_eq!(migrated.trusted_devices, legacy_state.trusted_devices);
        assert_eq!(migrated.sessions, legacy_state.sessions);
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn old_sqlite_state_without_schema_meta_is_rejected_when_it_has_content() {
        let state_path = temp_path("old-sqlite-state.json");
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let conn = open_state_connection(&sqlite_path).unwrap();
        initialize_daemon_state_schema(&conn, &sqlite_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO trusted_devices (
                device_id,
                public_key,
                trusted_at_ms,
                last_seen_at_ms,
                label
            )
            VALUES (?1, ?2, ?3, NULL, NULL)
            "#,
            params![DeviceId::new().0.to_string(), "device-public", 1_i64],
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO runtime_sessions (
                session_id,
                state,
                rows,
                cols,
                pixel_width,
                pixel_height,
                created_at_ms,
                updated_at_ms,
                restore_kind,
                restore_value
            )
            VALUES (?1, 'running', 24, 80, 0, 0, 1, 1, 'unix_socket', '{}')
            "#,
            params![SessionId::new().0.to_string()],
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO http_uploads (
                upload_id,
                target_path,
                size_bytes,
                dev,
                ino,
                updated_at_ms
            )
            VALUES ('upload-old', '/tmp/target.bin', '1', '1', '1', 1)
            "#,
            [],
        )
        .unwrap();
        drop(conn);

        let error = StateStore::load(&state_path).unwrap_err();

        assert!(matches!(
            error,
            StateError::IncompatibleVersion {
                found: None,
                expected: STATE_SCHEMA_VERSION,
                ..
            }
        ));
        cleanup_state_paths(&state_path);
    }

    #[test]
    fn sqlite_runtime_sessions_prevent_legacy_json_fallback() {
        let state_path = temp_path("sqlite-runtime-wins.json");
        let mut sqlite_state = sample_state();
        sqlite_state.daemon_identity = None;
        sqlite_state.trusted_devices.clear();
        let legacy_state = DaemonState::default();

        StateStore::save(&state_path, &sqlite_state).unwrap();
        fs::write(
            &state_path,
            serde_json::to_string_pretty(&legacy_state).unwrap(),
        )
        .unwrap();

        let loaded = StateStore::load(&state_path).unwrap();

        assert_eq!(loaded.sessions, sqlite_state.sessions);
        cleanup_state_paths(&state_path);
    }

    fn sample_state() -> DaemonState {
        let device_id = DeviceId::new();

        DaemonState {
            version: STATE_SCHEMA_VERSION,
            daemon_identity: Some(DaemonIdentitySnapshot {
                server_id: ServerId::new(),
                public_key: PublicKey("daemon-public".to_owned()),
                private_key: Some("ed25519-v1:daemon-private".to_owned()),
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
                restore_info: Some(PtyRestoreInfo::UnixSocket {
                    socket_path: PathBuf::from("/tmp/termd-test.sock"),
                    supervisor_pid: 42,
                    supervisor_status: PtySupervisorStatus::Running,
                }),
            }],
        }
    }

    fn cleanup_state_paths(state_path: &Path) {
        let sqlite_path = sqlite_state_path_for_state_path(state_path);
        let _ = fs::remove_file(state_path);
        for sidecar_path in sqlite_sidecar_paths(&sqlite_path) {
            let _ = fs::remove_file(sidecar_path);
        }
        let _ = fs::remove_file(&sqlite_path);
        let _ = fs::remove_dir(&sqlite_path);
        if let Some(parent) = state_path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    fn cleanup_temp_file(path: &Path) {
        let _ = fs::remove_file(path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    fn sqlite_sidecar_paths(sqlite_path: &Path) -> [PathBuf; 2] {
        [
            sqlite_sidecar_path(sqlite_path, "-wal"),
            sqlite_sidecar_path(sqlite_path, "-shm"),
        ]
    }

    fn sqlite_sidecar_path(sqlite_path: &Path, suffix: &str) -> PathBuf {
        let mut path = sqlite_path.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }

    #[cfg(unix)]
    fn force_wal_sidecars(sqlite_path: &Path) -> Connection {
        let conn = Connection::open(sqlite_path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA wal_autocheckpoint = 0;
            CREATE TABLE IF NOT EXISTS permission_probe (
                id INTEGER PRIMARY KEY
            );
            INSERT INTO permission_probe DEFAULT VALUES;
            "#,
        )
        .unwrap();
        for sidecar_path in sqlite_sidecar_paths(sqlite_path) {
            assert!(
                sidecar_path.exists(),
                "missing sidecar {}",
                sidecar_path.display()
            );
        }
        conn
    }

    #[cfg(unix)]
    fn unix_mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_unix_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn chown_path(path: &Path, uid: u32, gid: u32) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::chown(path.as_ptr(), uid, gid) };
        assert_eq!(result, 0, "chown failed: {}", io::Error::last_os_error());
    }

    #[cfg(unix)]
    fn assert_unsafe_sqlite_path(
        error: StateError,
        expected_path: &Path,
        expected_kind: UnsafeSqlitePathKind,
    ) {
        match error {
            StateError::UnsafeSqlitePath { path, kind } => {
                assert_eq!(path, expected_path);
                assert_eq!(kind, expected_kind);
            }
            other => panic!("expected UnsafeSqlitePath, got {other:?}"),
        }
    }
}
