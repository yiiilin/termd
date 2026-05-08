//! termd daemon 的 SQLite client history 存储。
//!
//! 这里保存 daemon 看到过的客户端历史，以及当前还在线的连接摘要。历史数据可能比 JSON 状态
//! 大得多，所以单独放进 SQLite，并且把连接开闭、attach/detach 做成事务。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use termd_proto::{ClientId, DeviceId, SessionId, UnixTimestampMillis};
use uuid::Uuid;

use super::StateError;

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
        let path = client_history_path_for_state_path(state_path.as_ref());
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

                CREATE INDEX IF NOT EXISTS idx_daemon_clients_order
                    ON daemon_clients(active_connection_count, connected_at_ms, device_id);
                CREATE INDEX IF NOT EXISTS idx_daemon_client_sessions_lookup
                    ON daemon_client_attached_sessions(device_id, session_id);
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

fn client_history_path_for_state_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("sqlite")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_tracks_current_connections_and_resets_runtime_state_on_open() {
        let state_path =
            std::env::temp_dir().join(format!("termd-client-history-{}.json", std::process::id()));
        let _ = fs::remove_file(client_history_path_for_state_path(&state_path));

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

        let _ = fs::remove_file(client_history_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }
}
