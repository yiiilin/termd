//! termd daemon 的 SQLite client history 存储。
//!
//! 这里保存 daemon 看到过的客户端历史，以及当前还在线的连接摘要。历史数据可能比 JSON 状态
//! 大得多，所以单独放进 SQLite，并且把连接开闭、attach/detach 做成事务。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use termd_proto::{ClientId, DeviceId, SessionId, SessionState, TerminalSize, UnixTimestampMillis};
use uuid::Uuid;

use super::{StateError, sqlite_state_path_for_state_path};

/// 客户端列表返回给协议层的持久化摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHistoryRecord {
    pub device_id: DeviceId,
    pub peer_ip: Option<String>,
    pub online: bool,
    pub connected_at_ms: UnixTimestampMillis,
    pub last_seen_at_ms: UnixTimestampMillis,
    pub attached_session_ids: Vec<SessionId>,
}

/// session 元数据的 daemon 端持久化摘要。
///
/// PTY 进程本身仍然是运行态资源，不能靠 SQLite 复活；这里保存的是 Web/CLI 多客户端需要共享的
/// session 名称、尺寸和文件树位置等元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHistoryRecord {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub state: SessionState,
    pub size: TerminalSize,
    pub root_path: String,
    pub files_path: Option<String>,
    pub created_at_ms: UnixTimestampMillis,
    pub updated_at_ms: UnixTimestampMillis,
}

/// SQLite 里的 client history store。
///
/// 这个 store 只保存 daemon 级历史和当前活跃连接的投影；runtime 仍然在协议层维护 PTY 与
/// session 状态机。
#[derive(Debug)]
pub struct ClientHistoryStore {
    path: PathBuf,
    conn: Connection,
}

impl ClientHistoryStore {
    /// 打开或创建 SQLite store，并把运行态字段重置为离线。
    pub fn open(state_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = sqlite_state_path_for_state_path(state_path.as_ref());
        ensure_parent_directory(&path)?;

        let conn = Connection::open(&path).map_err(|source| sqlite_error(&path, source))?;
        let mut store = Self { path, conn };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&mut self) -> Result<(), StateError> {
        self.conn
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;

                CREATE TABLE IF NOT EXISTS daemon_clients (
                    device_id TEXT PRIMARY KEY,
                    peer_ip TEXT,
                    connected_at_ms INTEGER NOT NULL,
                    last_seen_at_ms INTEGER NOT NULL,
                    active_connection_count INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS daemon_client_attached_sessions (
                    device_id TEXT NOT NULL,
                    connection_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    PRIMARY KEY (device_id, connection_id, session_id)
                );

                CREATE TABLE IF NOT EXISTS daemon_sessions (
                    session_id TEXT PRIMARY KEY,
                    name TEXT,
                    state TEXT NOT NULL,
                    rows INTEGER NOT NULL,
                    cols INTEGER NOT NULL,
                    pixel_width INTEGER NOT NULL,
                    pixel_height INTEGER NOT NULL,
                    root_path TEXT NOT NULL,
                    files_path TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_daemon_clients_order
                    ON daemon_clients(active_connection_count, connected_at_ms, device_id);
                CREATE INDEX IF NOT EXISTS idx_daemon_client_sessions_lookup
                    ON daemon_client_attached_sessions(device_id, session_id);
                CREATE INDEX IF NOT EXISTS idx_daemon_sessions_state
                    ON daemon_sessions(state, created_at_ms, session_id);
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;

        self.reset_runtime_state()?;
        Ok(())
    }

    /// 启动时把活跃连接状态清成空，这样重启后不会把旧进程残留的在线状态误认为真实在线。
    fn reset_runtime_state(&mut self) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute("UPDATE daemon_clients SET active_connection_count = 0", [])
            .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute("DELETE FROM daemon_client_attached_sessions", [])
            .map_err(|source| sqlite_error(&self.path, source))?;
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 记录新建 session 的共享元数据。
    pub fn record_session_created(
        &mut self,
        session_id: SessionId,
        state: SessionState,
        size: TerminalSize,
        root_path: impl AsRef<Path>,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        let root_path = root_path.as_ref().to_string_lossy().to_string();

        self.conn
            .execute(
                r#"
                INSERT INTO daemon_sessions (
                    session_id,
                    name,
                    state,
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                    root_path,
                    files_path,
                    created_at_ms,
                    updated_at_ms
                )
                VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?8)
                ON CONFLICT(session_id) DO UPDATE SET
                    state = excluded.state,
                    rows = excluded.rows,
                    cols = excluded.cols,
                    pixel_width = excluded.pixel_width,
                    pixel_height = excluded.pixel_height,
                    root_path = excluded.root_path,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![
                    session_id_text(session_id),
                    session_state_text(state),
                    i64::from(size.rows),
                    i64::from(size.cols),
                    i64::from(size.pixel_width),
                    i64::from(size.pixel_height),
                    root_path,
                    now_ms.0 as i64,
                ],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 持久化 session 尺寸，供列表和重连后的 UI 元数据使用。
    pub fn record_session_resized(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        self.conn
            .execute(
                r#"
                UPDATE daemon_sessions
                SET rows = ?1,
                    cols = ?2,
                    pixel_width = ?3,
                    pixel_height = ?4,
                    updated_at_ms = ?5
                WHERE session_id = ?6
                "#,
                params![
                    i64::from(size.rows),
                    i64::from(size.cols),
                    i64::from(size.pixel_width),
                    i64::from(size.pixel_height),
                    now_ms.0 as i64,
                    session_id_text(session_id),
                ],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 持久化 session 展示名；空值表示回到默认短 id 展示。
    pub fn record_session_renamed(
        &mut self,
        session_id: SessionId,
        name: Option<&str>,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        self.conn
            .execute(
                r#"
                UPDATE daemon_sessions
                SET name = ?1,
                    updated_at_ms = ?2
                WHERE session_id = ?3
                "#,
                params![name, now_ms.0 as i64, session_id_text(session_id)],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 关闭 session 时保留历史行，但默认列表会过滤 closed。
    pub fn record_session_closed(
        &mut self,
        session_id: SessionId,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        self.conn
            .execute(
                r#"
                UPDATE daemon_sessions
                SET state = ?1,
                    updated_at_ms = ?2
                WHERE session_id = ?3
                "#,
                params![
                    session_state_text(SessionState::Closed),
                    now_ms.0 as i64,
                    session_id_text(session_id),
                ],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 持久化 session 级文件树当前位置；多个 Web 客户端共享这一份状态。
    pub fn record_session_files_path(
        &mut self,
        session_id: SessionId,
        files_path: impl AsRef<Path>,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        let files_path = files_path.as_ref().to_string_lossy().to_string();

        self.conn
            .execute(
                r#"
                UPDATE daemon_sessions
                SET files_path = ?1,
                    updated_at_ms = ?2
                WHERE session_id = ?3
                "#,
                params![files_path, now_ms.0 as i64, session_id_text(session_id)],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 读取 session 级文件树当前位置。
    pub fn session_files_path(&self, session_id: SessionId) -> Result<Option<String>, StateError> {
        let path = self
            .conn
            .query_row(
                "SELECT files_path FROM daemon_sessions WHERE session_id = ?1",
                params![session_id_text(session_id)],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))?
            .flatten();

        Ok(path)
    }

    /// 返回仍处于可见状态的 session 元数据。
    pub fn list_sessions(&self) -> Result<Vec<SessionHistoryRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                r#"
                SELECT
                    session_id,
                    name,
                    state,
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                    root_path,
                    files_path,
                    created_at_ms,
                    updated_at_ms
                FROM daemon_sessions
                WHERE state != ?1
                ORDER BY created_at_ms, session_id
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map(params![session_state_text(SessionState::Closed)], |row| {
                session_history_record_from_row(row)
            })
            .map_err(|source| sqlite_error(&self.path, source))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|source| sqlite_error(&self.path, source))?);
        }

        Ok(sessions)
    }

    /// 记录某个设备刚刚建立了一条连接。
    pub fn record_connection(
        &mut self,
        device_id: DeviceId,
        peer_ip: Option<&str>,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;

        tx.execute(
            r#"
            INSERT INTO daemon_clients (
                device_id,
                peer_ip,
                connected_at_ms,
                last_seen_at_ms,
                active_connection_count
            )
            VALUES (?1, ?2, ?3, ?3, 1)
            ON CONFLICT(device_id) DO UPDATE SET
                peer_ip = excluded.peer_ip,
                last_seen_at_ms = excluded.last_seen_at_ms,
                active_connection_count = daemon_clients.active_connection_count + 1
            "#,
            params![device_id_text(device_id), peer_ip, now_ms.0 as i64],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;

        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 记录某条连接当前 attach 到哪个 session。
    pub fn record_attach(
        &mut self,
        device_id: DeviceId,
        connection_id: ClientId,
        session_id: SessionId,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;

        tx.execute(
            "UPDATE daemon_clients SET last_seen_at_ms = ?1 WHERE device_id = ?2",
            params![now_ms.0 as i64, device_id_text(device_id)],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute(
            r#"
            INSERT OR IGNORE INTO daemon_client_attached_sessions (
                device_id,
                connection_id,
                session_id
            )
            VALUES (?1, ?2, ?3)
            "#,
            params![
                device_id_text(device_id),
                connection_id_text(connection_id),
                session_id_text(session_id),
            ],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;

        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 断开某条连接，并删除它对应的 active attachment。
    pub fn record_disconnect(
        &mut self,
        device_id: DeviceId,
        connection_id: ClientId,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;

        tx.execute(
            "DELETE FROM daemon_client_attached_sessions WHERE connection_id = ?1",
            params![connection_id_text(connection_id)],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute(
            r#"
            UPDATE daemon_clients
            SET last_seen_at_ms = ?1,
                active_connection_count = CASE
                    WHEN active_connection_count > 0 THEN active_connection_count - 1
                    ELSE 0
                END
            WHERE device_id = ?2
            "#,
            params![now_ms.0 as i64, device_id_text(device_id)],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;

        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 判断某个设备当前是否还有活跃 session。
    pub fn device_has_active_session(
        &self,
        device_id: DeviceId,
        session_id: SessionId,
    ) -> Result<bool, StateError> {
        let exists = self
            .conn
            .query_row(
                r#"
                SELECT 1
                FROM daemon_client_attached_sessions
                WHERE device_id = ?1 AND session_id = ?2
                LIMIT 1
                "#,
                params![device_id_text(device_id), session_id_text(session_id)],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))?;

        Ok(exists.is_some())
    }

    /// 删除某个 session 的所有活跃 attachment。
    ///
    /// session close 后它不应再出现在 daemon 客户端的当前 attach 列表里；历史 client 行仍保留。
    pub fn remove_session_attachments(&mut self, session_id: SessionId) -> Result<(), StateError> {
        self.conn
            .execute(
                "DELETE FROM daemon_client_attached_sessions WHERE session_id = ?1",
                params![session_id_text(session_id)],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    /// 返回当前 daemon 看到过的所有客户端摘要。
    pub fn list_clients(&self) -> Result<Vec<ClientHistoryRecord>, StateError> {
        let mut attached_session_ids: HashMap<DeviceId, HashSet<SessionId>> = HashMap::new();

        {
            let mut stmt = self
                .conn
                .prepare(
                    r#"
                    SELECT device_id, session_id
                    FROM daemon_client_attached_sessions
                    ORDER BY device_id, session_id
                    "#,
                )
                .map_err(|source| sqlite_error(&self.path, source))?;
            let rows = stmt
                .query_map([], |row| {
                    let device_id = parse_device_id(row.get::<_, String>(0)?)?;
                    let session_id = parse_session_id(row.get::<_, String>(1)?)?;
                    Ok((device_id, session_id))
                })
                .map_err(|source| sqlite_error(&self.path, source))?;

            for row in rows {
                let (device_id, session_id) =
                    row.map_err(|source| sqlite_error(&self.path, source))?;
                attached_session_ids
                    .entry(device_id)
                    .or_default()
                    .insert(session_id);
            }
        }

        let mut clients = Vec::new();
        let mut stmt = self
            .conn
            .prepare(
                r#"
                SELECT device_id, peer_ip, connected_at_ms, last_seen_at_ms, active_connection_count
                FROM daemon_clients
                ORDER BY connected_at_ms, device_id
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map([], |row| {
                let device_id = parse_device_id(row.get::<_, String>(0)?)?;
                let peer_ip = row.get::<_, Option<String>>(1)?;
                let connected_at_ms = UnixTimestampMillis(row.get::<_, i64>(2)? as u64);
                let last_seen_at_ms = UnixTimestampMillis(row.get::<_, i64>(3)? as u64);
                let active_connection_count = row.get::<_, i64>(4)?;

                Ok(ClientHistoryRow {
                    device_id,
                    peer_ip,
                    connected_at_ms,
                    last_seen_at_ms,
                    active_connection_count,
                })
            })
            .map_err(|source| sqlite_error(&self.path, source))?;

        for row in rows {
            let row = row.map_err(|source| sqlite_error(&self.path, source))?;
            let mut sessions: Vec<_> = attached_session_ids
                .get(&row.device_id)
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default();
            sessions.sort_by_key(|session_id| session_id.0);

            clients.push(ClientHistoryRecord {
                device_id: row.device_id,
                peer_ip: row.peer_ip,
                online: row.active_connection_count > 0,
                connected_at_ms: row.connected_at_ms,
                last_seen_at_ms: row.last_seen_at_ms,
                attached_session_ids: sessions,
            });
        }

        Ok(clients)
    }
}

#[derive(Debug)]
struct ClientHistoryRow {
    device_id: DeviceId,
    peer_ip: Option<String>,
    connected_at_ms: UnixTimestampMillis,
    last_seen_at_ms: UnixTimestampMillis,
    active_connection_count: i64,
}

fn session_history_record_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionHistoryRecord> {
    let session_id = parse_session_id(row.get::<_, String>(0)?)?;
    let name = row.get::<_, Option<String>>(1)?;
    let state = parse_session_state(row.get::<_, String>(2)?)?;
    let rows = integer_to_u16(row.get::<_, i64>(3)?, 3)?;
    let cols = integer_to_u16(row.get::<_, i64>(4)?, 4)?;
    let pixel_width = integer_to_u16(row.get::<_, i64>(5)?, 5)?;
    let pixel_height = integer_to_u16(row.get::<_, i64>(6)?, 6)?;
    let root_path = row.get::<_, String>(7)?;
    let files_path = row.get::<_, Option<String>>(8)?;
    let created_at_ms = UnixTimestampMillis(row.get::<_, i64>(9)? as u64);
    let updated_at_ms = UnixTimestampMillis(row.get::<_, i64>(10)? as u64);

    Ok(SessionHistoryRecord {
        session_id,
        name,
        state,
        size: TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
        },
        root_path,
        files_path,
        created_at_ms,
        updated_at_ms,
    })
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

fn device_id_text(device_id: DeviceId) -> String {
    device_id.0.to_string()
}

fn connection_id_text(connection_id: ClientId) -> String {
    connection_id.0.to_string()
}

fn session_id_text(session_id: SessionId) -> String {
    session_id.0.to_string()
}

fn session_state_text(state: SessionState) -> &'static str {
    match state {
        SessionState::Created => "created",
        SessionState::Running => "running",
        SessionState::Closed => "closed",
    }
}

fn parse_device_id(raw: String) -> rusqlite::Result<DeviceId> {
    Uuid::parse_str(&raw).map(DeviceId).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source))
    })
}

fn parse_session_id(raw: String) -> rusqlite::Result<SessionId> {
    Uuid::parse_str(&raw).map(SessionId).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(source))
    })
}

fn parse_session_state(raw: String) -> rusqlite::Result<SessionState> {
    match raw.as_str() {
        "created" => Ok(SessionState::Created),
        "running" => Ok(SessionState::Running),
        "closed" => Ok(SessionState::Closed),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Text,
            format!("unknown session state: {raw}").into(),
        )),
    }
}

fn integer_to_u16(value: i64, column: usize) -> rusqlite::Result<u16> {
    u16::try_from(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(source))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_tracks_current_connections_and_resets_runtime_state_on_open() {
        let state_path =
            std::env::temp_dir().join(format!("termd-client-history-{}.json", std::process::id()));
        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));

        let device_id = DeviceId::new();
        let session_id = SessionId::new();
        let connection_id = ClientId::new();
        let now_ms = UnixTimestampMillis(1_710_000_000_000);

        {
            let mut store = ClientHistoryStore::open(&state_path).unwrap();
            store
                .record_connection(device_id, Some("192.0.2.10"), now_ms)
                .unwrap();
            store
                .record_attach(device_id, connection_id, session_id, now_ms)
                .unwrap();

            let clients = store.list_clients().unwrap();
            assert_eq!(clients.len(), 1);
            assert!(clients[0].online);
            assert_eq!(clients[0].attached_session_ids, vec![session_id]);
        }

        {
            let store = ClientHistoryStore::open(&state_path).unwrap();
            let clients = store.list_clients().unwrap();
            assert_eq!(clients.len(), 1);
            assert!(!clients[0].online);
            assert!(clients[0].attached_session_ids.is_empty());
        }

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn store_persists_session_metadata_and_file_tree_path() {
        let state_path =
            std::env::temp_dir().join(format!("termd-session-store-{}.json", std::process::id()));
        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));

        let session_id = SessionId::new();
        let now_ms = UnixTimestampMillis(1_710_000_000_000);

        {
            let mut store = ClientHistoryStore::open(&state_path).unwrap();
            store
                .record_session_created(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(24, 80),
                    "/home/me",
                    now_ms,
                )
                .unwrap();
            store
                .record_session_renamed(
                    session_id,
                    Some("work shell"),
                    UnixTimestampMillis(now_ms.0 + 1),
                )
                .unwrap();
            store
                .record_session_files_path(
                    session_id,
                    "/home/me/project",
                    UnixTimestampMillis(now_ms.0 + 2),
                )
                .unwrap();

            let sessions = store.list_sessions().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, session_id);
            assert_eq!(sessions[0].name.as_deref(), Some("work shell"));
            assert_eq!(sessions[0].root_path, "/home/me");
            assert_eq!(sessions[0].files_path.as_deref(), Some("/home/me/project"));
        }

        {
            let store = ClientHistoryStore::open(&state_path).unwrap();
            assert_eq!(
                store.session_files_path(session_id).unwrap().as_deref(),
                Some("/home/me/project")
            );
        }

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }
}
