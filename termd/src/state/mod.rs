//! termd daemon 的本地持久状态快照。
//!
//! 本模块保存 daemon 需要跨进程重启保留的最小事实：daemon 公共身份快照、可信设备清单、
//! session 元数据，以及 SQLite client history 存储入口。这里刻意不保存 PTY 明文输出、
//! terminal 历史或文件传输内容，也不引入账号体系。

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use termd_proto::{
    DeviceId, PublicKey, ServerId, SessionId, SessionState, TerminalSize, UnixTimestampMillis,
};
use uuid::Uuid;

use crate::pty::{PtyRestoreInfo, PtySupervisorStatus};

pub mod client_history;

/// 当前 daemon 状态文件的 schema 版本。
pub const STATE_SCHEMA_VERSION: u32 = 1;

const META_SERVER_ID: &str = "server_id";
const META_DAEMON_PUBLIC_KEY: &str = "daemon_public_key";
const META_DAEMON_PRIVATE_KEY: &str = "daemon_private_key";

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
    /// 从 SQLite 状态库读取 `DaemonState`。
    ///
    /// 旧版本的 `daemon-state.json` 只作为迁移来源读取；SQLite 一旦有 daemon 状态，
    /// 后续启动都以 SQLite 为准，避免 stale JSON 覆盖新信任数据。
    pub fn load(path: impl AsRef<Path>) -> Result<DaemonState, StateError> {
        let path = path.as_ref();
        let sqlite_path = sqlite_state_path_for_state_path(path);
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        let state = load_sqlite_state(&conn, &sqlite_path)?;

        if state.daemon_identity.is_some()
            || !state.trusted_devices.is_empty()
            || !state.sessions.is_empty()
        {
            return Ok(state);
        }

        load_legacy_json_state(path, &sqlite_path).map(|legacy| legacy.unwrap_or(state))
    }

    /// 将 daemon 状态保存到 SQLite 状态库。
    ///
    /// 这里不再写 `daemon-state.json`；旧 JSON 文件即使存在，也只作为迁移来源。
    pub fn save(path: impl AsRef<Path>, state: &DaemonState) -> Result<(), StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let mut conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
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
        let conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        let updated = conn
            .execute(
                r#"
                UPDATE runtime_sessions
                SET state = ?1,
                    updated_at_ms = ?2,
                    restore_kind = NULL,
                    restore_value = NULL
                WHERE session_id = ?3
                "#,
                params![
                    session_state_text(SessionState::Closed),
                    now_ms.0 as i64,
                    session_id.0.to_string(),
                ],
            )
            .map_err(|source| sqlite_error(&sqlite_path, source))?
            > 0;
        Ok(updated)
    }

    /// 清理已经不可恢复的 closed runtime 行；仍有 live supervisor 的 id 必须保留。
    pub fn prune_closed_sessions_except(
        path: impl AsRef<Path>,
        protected_session_ids: &HashSet<SessionId>,
    ) -> Result<usize, StateError> {
        let sqlite_path = sqlite_state_path_for_state_path(path.as_ref());
        let mut conn = open_state_connection(&sqlite_path)?;
        initialize_daemon_state_schema(&conn, &sqlite_path)?;
        prune_closed_runtime_sessions_except(&mut conn, &sqlite_path, protected_session_ids)
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
    InvalidDaemonIdentity {
        source: String,
    },
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
            Self::InvalidDaemonIdentity { .. } => write!(f, "failed to restore daemon identity"),
        }
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse { source, .. } => Some(source),
            Self::Read { source, .. } | Self::CreateDirectory { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            Self::InvalidDaemonIdentity { .. } => None,
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
        Ok(raw) => parse_json(path, &raw).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StateError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn open_state_connection(sqlite_path: &Path) -> Result<Connection, StateError> {
    ensure_parent_directory(sqlite_path)?;
    Connection::open(sqlite_path).map_err(|source| sqlite_error(sqlite_path, source))
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
        "#,
    )
    .map_err(|source| sqlite_error(path, source))
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
            let trusted_at_ms = UnixTimestampMillis(row.get::<_, i64>(2)? as u64);
            let last_seen_at_ms = row
                .get::<_, Option<i64>>(3)?
                .map(|value| UnixTimestampMillis(value as u64));
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
                rows: row.get::<_, i64>(2)? as u16,
                cols: row.get::<_, i64>(3)? as u16,
                pixel_width: row.get::<_, i64>(4)? as u16,
                pixel_height: row.get::<_, i64>(5)? as u16,
            };
            let restore_kind = row.get::<_, Option<String>>(8)?;
            let restore_value = row.get::<_, Option<String>>(9)?;
            let restore_info = parse_restore_info(restore_kind, restore_value)?;

            Ok(SessionStateRecord {
                session_id,
                state,
                size,
                created_at_ms: UnixTimestampMillis(row.get::<_, i64>(6)? as u64),
                updated_at_ms: UnixTimestampMillis(row.get::<_, i64>(7)? as u64),
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
    let now_ms = current_unix_timestamp_millis().0 as i64;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| sqlite_error(path, source))?;

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
        let (restore_kind, restore_value) = serialize_restore_info(session.restore_info.as_ref());
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
                state = excluded.state,
                rows = excluded.rows,
                cols = excluded.cols,
                pixel_width = excluded.pixel_width,
                pixel_height = excluded.pixel_height,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms,
                restore_kind = excluded.restore_kind,
                restore_value = excluded.restore_value
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

fn session_state_text(state: SessionState) -> &'static str {
    match state {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::Closed => "closed",
    }
}

fn serialize_restore_info(
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
        None => (None, None),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedUnixSocketRestoreInfo {
    socket_path: PathBuf,
    supervisor_pid: u32,
    supervisor_status: PtySupervisorStatus,
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
    use std::path::{Path, PathBuf};
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
        let _ = fs::remove_file(&sqlite_path);
        let _ = fs::remove_file(sqlite_path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(sqlite_path.with_extension("sqlite-shm"));
    }
}
