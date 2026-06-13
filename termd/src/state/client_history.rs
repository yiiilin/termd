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

use super::{StateError, StateStore, sqlite_state_path_for_state_path, write_sqlite_state_version};

/// 客户端列表返回给协议层的持久化摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHistoryRecord {
    pub device_id: DeviceId,
    pub name: Option<String>,
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
    pub display_order: i64,
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
        StateStore::ensure_compatible(state_path.as_ref())?;
        let path = sqlite_state_path_for_state_path(state_path.as_ref());
        ensure_parent_directory(&path)?;

        let conn = Connection::open(&path).map_err(|source| sqlite_error(&path, source))?;
        let mut store = Self { path, conn };
        store.initialize()?;
        write_sqlite_state_version(&store.conn, &store.path)?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn set_query_only_for_test(&mut self, enabled: bool) -> Result<(), StateError> {
        // 中文注释：协议层 rollback 测试需要稳定制造“runtime attach 已完成，
        // 但后续历史投影写入失败”的窗口。SQLite query_only 只影响当前连接，
        // 不需要修改文件权限，测试结束后也能明确恢复。
        let pragma = if enabled {
            "PRAGMA query_only = ON;"
        } else {
            "PRAGMA query_only = OFF;"
        };
        self.conn
            .execute_batch(pragma)
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
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
                    name TEXT,
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
                    display_order INTEGER NOT NULL DEFAULT 0,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_daemon_clients_order
                    ON daemon_clients(active_connection_count, connected_at_ms, device_id);
                CREATE INDEX IF NOT EXISTS idx_daemon_client_sessions_lookup
                    ON daemon_client_attached_sessions(device_id, session_id);
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;

        self.ensure_daemon_clients_name_column()?;
        self.ensure_daemon_sessions_display_order_column()?;
        self.ensure_daemon_sessions_display_order_index()?;
        self.reset_runtime_state()?;
        Ok(())
    }

    fn ensure_daemon_clients_name_column(&self) -> Result<(), StateError> {
        if self.column_exists("daemon_clients", "name")? {
            return Ok(());
        }

        // 旧版 SQLite 没有客户端展示名列；在线历史保留，后续连接会自动补充 name。
        self.conn
            .execute("ALTER TABLE daemon_clients ADD COLUMN name TEXT", [])
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    fn ensure_daemon_sessions_display_order_column(&self) -> Result<(), StateError> {
        if self.column_exists("daemon_sessions", "display_order")? {
            return Ok(());
        }

        // 旧版只按 created_at 排序；迁移时把现有行固化成 display_order，
        // 后续拖拽排序就有 daemon 端权威状态，不再依赖某个浏览器的内存数组。
        self.conn
            .execute(
                "ALTER TABLE daemon_sessions ADD COLUMN display_order INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let mut stmt = self
            .conn
            .prepare(
                r#"
                SELECT session_id
                FROM daemon_sessions
                ORDER BY created_at_ms DESC, session_id DESC
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| sqlite_error(&self.path, source))?;
        let mut session_ids = Vec::new();
        for row in rows {
            session_ids.push(row.map_err(|source| sqlite_error(&self.path, source))?);
        }
        drop(stmt);
        for (index, session_id) in session_ids.into_iter().enumerate() {
            self.conn
                .execute(
                    "UPDATE daemon_sessions SET display_order = ?1 WHERE session_id = ?2",
                    params![index as i64, session_id],
                )
                .map_err(|source| sqlite_error(&self.path, source))?;
        }
        Ok(())
    }

    fn ensure_daemon_sessions_display_order_index(&self) -> Result<(), StateError> {
        // display_order 是后加列；旧库初始化时必须先完成列迁移，再重建依赖该列的索引。
        self.conn
            .execute_batch(
                r#"
                DROP INDEX IF EXISTS idx_daemon_sessions_state;
                CREATE INDEX idx_daemon_sessions_state
                    ON daemon_sessions(state, display_order, created_at_ms, session_id);
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(())
    }

    fn column_exists(&self, table: &str, column: &str) -> Result<bool, StateError> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|source| sqlite_error(&self.path, source))?;

        for row in rows {
            if row.map_err(|source| sqlite_error(&self.path, source))? == column {
                return Ok(true);
            }
        }
        Ok(false)
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
        name: Option<&str>,
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
                    display_order,
                    created_at_ms,
                    updated_at_ms
                )
                VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL,
                    COALESCE((SELECT MIN(display_order) - 1 FROM daemon_sessions), 0),
                    ?9, ?9
                )
                ON CONFLICT(session_id) DO UPDATE SET
                    name = COALESCE(daemon_sessions.name, excluded.name),
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
                    name,
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

    /// 同步 session 的运行态投影。
    ///
    /// PTY runtime 才是 `created -> running` 的事实来源；这里把 attach 后的状态写回
    /// daemon 展示表，避免重启/升级前检查看到 live supervisor 但元数据仍停在 created。
    pub fn record_session_runtime_state(
        &mut self,
        session_id: SessionId,
        state: SessionState,
        size: TerminalSize,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        self.conn
            .execute(
                r#"
                UPDATE daemon_sessions
                SET state = ?1,
                    rows = ?2,
                    cols = ?3,
                    pixel_width = ?4,
                    pixel_height = ?5,
                    updated_at_ms = ?6
                WHERE session_id = ?7
                  AND state != ?8
                "#,
                params![
                    session_state_text(state),
                    i64::from(size.rows),
                    i64::from(size.cols),
                    i64::from(size.pixel_width),
                    i64::from(size.pixel_height),
                    now_ms.0 as i64,
                    session_id_text(session_id),
                    session_state_text(SessionState::Closed),
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

    /// 删除单个已关闭 session 的展示历史。调用方必须先确认对应 supervisor 已经结束。
    pub fn prune_closed_session(&mut self, session_id: SessionId) -> Result<bool, StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute(
            "DELETE FROM daemon_client_attached_sessions WHERE session_id = ?1",
            params![session_id_text(session_id)],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        let deleted = tx
            .execute(
                "DELETE FROM daemon_sessions WHERE session_id = ?1 AND state = ?2",
                params![
                    session_id_text(session_id),
                    session_state_text(SessionState::Closed)
                ],
            )
            .map_err(|source| sqlite_error(&self.path, source))?
            > 0;
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(deleted)
    }

    /// 清理已经不可恢复的 closed 展示行；仍有 live supervisor 的 id 必须保留。
    pub fn prune_closed_sessions_except(
        &mut self,
        protected_session_ids: &HashSet<SessionId>,
    ) -> Result<usize, StateError> {
        let protected = protected_session_ids
            .iter()
            .map(|session_id| session_id_text(*session_id))
            .collect::<HashSet<_>>();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        let mut stmt = tx
            .prepare("SELECT session_id FROM daemon_sessions WHERE state = ?1")
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map(params![session_state_text(SessionState::Closed)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|source| sqlite_error(&self.path, source))?;
        let mut deletable = Vec::new();
        for row in rows {
            let session_id = row.map_err(|source| sqlite_error(&self.path, source))?;
            if !protected.contains(&session_id) {
                deletable.push(session_id);
            }
        }
        drop(stmt);

        let mut deleted = 0;
        for session_id in deletable {
            tx.execute(
                "DELETE FROM daemon_client_attached_sessions WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
            // closed 且没有 live supervisor 保护的展示行已经不可打开，保留只会污染统计。
            deleted += tx
                .execute(
                    "DELETE FROM daemon_sessions WHERE session_id = ?1 AND state = ?2",
                    params![session_id, session_state_text(SessionState::Closed)],
                )
                .map_err(|source| sqlite_error(&self.path, source))?;
        }
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(deleted)
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

    /// 持久化 daemon 端的 session 列表顺序。
    ///
    /// 前端拖拽排序属于跨客户端共享状态：重启后必须由 daemon 返回同一顺序，而不是由某个浏览器
    /// 的本地数组临时决定。
    pub fn record_session_order(
        &mut self,
        session_ids: &[SessionId],
        now_ms: UnixTimestampMillis,
    ) -> Result<Vec<SessionId>, StateError> {
        let known_sessions = self.list_sessions()?;
        let known_by_id = known_sessions
            .iter()
            .map(|record| (record.session_id, record.display_order))
            .collect::<HashMap<_, _>>();
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for session_id in session_ids {
            if !known_by_id.contains_key(session_id) || !seen.insert(*session_id) {
                continue;
            }
            ordered.push(*session_id);
        }

        let requested = ordered.iter().copied().collect::<HashSet<_>>();
        let mut remaining = known_sessions
            .into_iter()
            .filter(|record| !requested.contains(&record.session_id))
            .collect::<Vec<_>>();
        remaining.sort_by(|left, right| {
            left.display_order
                .cmp(&right.display_order)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        ordered.extend(remaining.into_iter().map(|record| record.session_id));

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        for (index, session_id) in ordered.iter().enumerate() {
            tx.execute(
                r#"
                UPDATE daemon_sessions
                SET display_order = ?1,
                    updated_at_ms = ?2
                WHERE session_id = ?3
                "#,
                params![index as i64, now_ms.0 as i64, session_id_text(*session_id),],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        }
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;

        Ok(ordered)
    }

    /// daemon 重启后修复可重连 session 的展示元数据。
    ///
    /// runtime_sessions 是 supervisor 是否可恢复的事实来源；如果 daemon_sessions 行缺失或仍是
    /// closed，这里用保守默认值补齐 Web/CLI 列表需要的 root、name 和 files_path。已有用户命名
    /// 和文件树位置不会被默认值覆盖。
    pub fn record_session_restored(
        &mut self,
        session_id: SessionId,
        state: SessionState,
        size: TerminalSize,
        root_path: impl AsRef<Path>,
        default_name: &str,
        files_path: impl AsRef<Path>,
        created_at_ms: UnixTimestampMillis,
        updated_at_ms: UnixTimestampMillis,
    ) -> Result<SessionHistoryRecord, StateError> {
        let root_path = root_path.as_ref().to_string_lossy().to_string();
        let files_path = files_path.as_ref().to_string_lossy().to_string();

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
                    display_order,
                    created_at_ms,
                    updated_at_ms
                )
                VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                    COALESCE((SELECT MAX(display_order) + 1 FROM daemon_sessions), 0),
                    ?10, ?11
                )
                ON CONFLICT(session_id) DO UPDATE SET
                    name = COALESCE(daemon_sessions.name, excluded.name),
                    state = excluded.state,
                    rows = excluded.rows,
                    cols = excluded.cols,
                    pixel_width = excluded.pixel_width,
                    pixel_height = excluded.pixel_height,
                    root_path = daemon_sessions.root_path,
                    files_path = COALESCE(daemon_sessions.files_path, excluded.files_path),
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![
                    session_id_text(session_id),
                    default_name,
                    session_state_text(state),
                    i64::from(size.rows),
                    i64::from(size.cols),
                    i64::from(size.pixel_width),
                    i64::from(size.pixel_height),
                    root_path,
                    files_path,
                    created_at_ms.0 as i64,
                    updated_at_ms.0 as i64,
                ],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;

        self.session_record(session_id)?
            .ok_or_else(|| sqlite_error(&self.path, rusqlite::Error::QueryReturnedNoRows))
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

    /// 按 id 读取单条 session 元数据；包含 closed 行。
    ///
    /// session 名称、root 和文件树路径是展示元数据，即使 runtime 状态曾被错误清理或标记
    /// closed，也要允许恢复路径拿回来，避免存活 supervisor 只能退回 `restored-*` 默认名。
    pub fn session_record_including_closed(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionHistoryRecord>, StateError> {
        self.conn
            .query_row(
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
                    display_order,
                    created_at_ms,
                    updated_at_ms
                FROM daemon_sessions
                WHERE session_id = ?1
                "#,
                params![session_id_text(session_id)],
                session_history_record_from_row,
            )
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))
    }

    fn session_record(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionHistoryRecord>, StateError> {
        self.session_record_including_closed(session_id)
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
                    display_order,
                    created_at_ms,
                    updated_at_ms
                FROM daemon_sessions
                WHERE state != ?1
                ORDER BY display_order, created_at_ms, session_id
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
        name: Option<&str>,
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
                name,
                peer_ip,
                connected_at_ms,
                last_seen_at_ms,
                active_connection_count
            )
            VALUES (?1, ?2, ?3, ?4, ?4, 1)
            ON CONFLICT(device_id) DO UPDATE SET
                name = COALESCE(excluded.name, daemon_clients.name),
                peer_ip = excluded.peer_ip,
                last_seen_at_ms = excluded.last_seen_at_ms,
                active_connection_count = daemon_clients.active_connection_count + 1
            "#,
            params![device_id_text(device_id), name, peer_ip, now_ms.0 as i64],
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

    /// 更新客户端展示名；只影响 UI 区分，不改变设备信任关系。
    pub fn record_client_name(
        &mut self,
        device_id: DeviceId,
        name: &str,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), StateError> {
        self.conn
            .execute(
                r#"
                UPDATE daemon_clients
                SET name = ?1,
                    last_seen_at_ms = ?2
                WHERE device_id = ?3
                "#,
                params![name, now_ms.0 as i64, device_id_text(device_id)],
            )
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

    /// 删除离线客户端历史记录；在线客户端仍由当前连接负责维护，不能在这里清掉。
    pub fn forget_offline_client(&mut self, device_id: DeviceId) -> Result<bool, StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        let active_connection_count = tx
            .query_row(
                "SELECT active_connection_count FROM daemon_clients WHERE device_id = ?1",
                params![device_id_text(device_id)],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))?;

        if active_connection_count.unwrap_or_default() > 0 {
            tx.commit()
                .map_err(|source| sqlite_error(&self.path, source))?;
            return Ok(false);
        }

        tx.execute(
            "DELETE FROM daemon_client_attached_sessions WHERE device_id = ?1",
            params![device_id_text(device_id)],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        let deleted = tx
            .execute(
                "DELETE FROM daemon_clients WHERE device_id = ?1",
                params![device_id_text(device_id)],
            )
            .map_err(|source| sqlite_error(&self.path, source))?
            > 0;
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(deleted)
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
                SELECT device_id, name, peer_ip, connected_at_ms, last_seen_at_ms, active_connection_count
                FROM daemon_clients
                ORDER BY connected_at_ms, device_id
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = stmt
            .query_map([], |row| {
                let device_id = parse_device_id(row.get::<_, String>(0)?)?;
                let name = row.get::<_, Option<String>>(1)?;
                let peer_ip = row.get::<_, Option<String>>(2)?;
                let connected_at_ms = UnixTimestampMillis(row.get::<_, i64>(3)? as u64);
                let last_seen_at_ms = UnixTimestampMillis(row.get::<_, i64>(4)? as u64);
                let active_connection_count = row.get::<_, i64>(5)?;

                Ok(ClientHistoryRow {
                    device_id,
                    name,
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
                name: row.name,
                peer_ip: row.peer_ip,
                online: row.active_connection_count > 0,
                connected_at_ms: row.connected_at_ms,
                last_seen_at_ms: row.last_seen_at_ms,
                attached_session_ids: sessions,
            });
        }

        Ok(clients)
    }

    #[cfg(test)]
    pub(crate) fn active_connection_count_for_test(
        &self,
        device_id: DeviceId,
    ) -> Result<Option<i64>, StateError> {
        let mut stmt = self
            .conn
            .prepare("SELECT active_connection_count FROM daemon_clients WHERE device_id = ?1")
            .map_err(|source| sqlite_error(&self.path, source))?;
        let count = stmt
            .query_row(params![device_id_text(device_id)], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(count)
    }
}

#[derive(Debug)]
struct ClientHistoryRow {
    device_id: DeviceId,
    name: Option<String>,
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
    let display_order = row.get::<_, i64>(9)?;
    let created_at_ms = UnixTimestampMillis(row.get::<_, i64>(10)? as u64);
    let updated_at_ms = UnixTimestampMillis(row.get::<_, i64>(11)? as u64);

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
        display_order,
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
                .record_connection(
                    device_id,
                    Some("Browser on Linux"),
                    Some("192.0.2.10"),
                    now_ms,
                )
                .unwrap();
            store
                .record_attach(device_id, connection_id, session_id, now_ms)
                .unwrap();

            let clients = store.list_clients().unwrap();
            assert_eq!(clients.len(), 1);
            assert!(clients[0].online);
            assert_eq!(clients[0].name.as_deref(), Some("Browser on Linux"));
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
                    Some("initial shell"),
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

    #[test]
    fn record_session_runtime_state_promotes_created_metadata_without_reopening_closed_rows() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-session-runtime-state-store-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let session_id = SessionId::new();

        {
            let mut store = ClientHistoryStore::open(&state_path).unwrap();
            store
                .record_session_created(
                    session_id,
                    SessionState::Created,
                    TerminalSize::new(24, 80),
                    Some("booting shell"),
                    "/home/me",
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
            store
                .record_session_runtime_state(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(30, 100),
                    UnixTimestampMillis(1_001),
                )
                .unwrap();

            let record = store
                .session_record_including_closed(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(record.state, SessionState::Running);
            assert_eq!(record.size, TerminalSize::new(30, 100));

            store
                .record_session_closed(session_id, UnixTimestampMillis(1_002))
                .unwrap();
            store
                .record_session_runtime_state(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(40, 120),
                    UnixTimestampMillis(1_003),
                )
                .unwrap();

            let closed = store
                .session_record_including_closed(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(closed.state, SessionState::Closed);
            assert_eq!(closed.size, TerminalSize::new(30, 100));
        }

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn store_persists_session_display_order_across_reopen() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-session-order-store-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let first_session_id = SessionId::new();
        let second_session_id = SessionId::new();
        let third_session_id = SessionId::new();

        {
            let mut store = ClientHistoryStore::open(&state_path).unwrap();
            for (index, session_id) in [first_session_id, second_session_id, third_session_id]
                .into_iter()
                .enumerate()
            {
                store
                    .record_session_created(
                        session_id,
                        SessionState::Running,
                        TerminalSize::new(24, 80),
                        Some(&format!("session-{index}")),
                        "/home/me",
                        UnixTimestampMillis(1_000 + index as u64),
                    )
                    .unwrap();
            }

            // 新建 session 默认排在列表最前面，避免用户开新 shell 后还要滚动寻找。
            assert_eq!(
                store
                    .list_sessions()
                    .unwrap()
                    .into_iter()
                    .map(|record| record.session_id)
                    .collect::<Vec<_>>(),
                vec![third_session_id, second_session_id, first_session_id]
            );

            let persisted_order = store
                .record_session_order(
                    &[first_session_id, third_session_id, second_session_id],
                    UnixTimestampMillis(2_000),
                )
                .unwrap();
            assert_eq!(
                persisted_order,
                vec![first_session_id, third_session_id, second_session_id]
            );
        }

        {
            let store = ClientHistoryStore::open(&state_path).unwrap();
            assert_eq!(
                store
                    .list_sessions()
                    .unwrap()
                    .into_iter()
                    .map(|record| record.session_id)
                    .collect::<Vec<_>>(),
                vec![first_session_id, third_session_id, second_session_id]
            );
        }

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn store_rejects_legacy_session_rows_without_state_schema_version() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-session-order-legacy-reject-{}.json",
            std::process::id()
        ));
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let _ = fs::remove_file(&sqlite_path);
        let old_session_id = SessionId::new();
        let new_session_id = SessionId::new();

        {
            let conn = Connection::open(&sqlite_path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE daemon_sessions (
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
                "#,
            )
            .unwrap();
            conn.execute(
                r#"
                INSERT INTO daemon_sessions (
                    session_id, name, state, rows, cols, pixel_width, pixel_height,
                    root_path, files_path, created_at_ms, updated_at_ms
                )
                VALUES (?1, 'old', 'running', 24, 80, 0, 0, '/home/me', NULL, 1000, 1000)
                "#,
                params![session_id_text(old_session_id)],
            )
            .unwrap();
            conn.execute(
                r#"
                INSERT INTO daemon_sessions (
                    session_id, name, state, rows, cols, pixel_width, pixel_height,
                    root_path, files_path, created_at_ms, updated_at_ms
                )
                VALUES (?1, 'new', 'running', 24, 80, 0, 0, '/home/me', NULL, 2000, 2000)
                "#,
                params![session_id_text(new_session_id)],
            )
            .unwrap();
        }

        let error = ClientHistoryStore::open(&state_path).unwrap_err();

        assert!(matches!(
            error,
            StateError::IncompatibleVersion {
                found: None,
                expected: super::super::STATE_SCHEMA_VERSION,
                ..
            }
        ));

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn store_patches_v2_session_rows_into_stable_display_order() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-session-order-v2-patch-{}.json",
            std::process::id()
        ));
        let sqlite_path = sqlite_state_path_for_state_path(&state_path);
        let _ = fs::remove_file(&sqlite_path);
        let old_session_id = SessionId::new();
        let new_session_id = SessionId::new();

        {
            let conn = Connection::open(&sqlite_path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE daemon_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE daemon_sessions (
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
                "#,
            )
            .unwrap();
            write_sqlite_state_version(&conn, &sqlite_path).unwrap();
            conn.execute(
                r#"
                INSERT INTO daemon_sessions (
                    session_id, name, state, rows, cols, pixel_width, pixel_height,
                    root_path, files_path, created_at_ms, updated_at_ms
                )
                VALUES (?1, 'old', 'running', 24, 80, 0, 0, '/home/me', NULL, 1000, 1000)
                "#,
                params![session_id_text(old_session_id)],
            )
            .unwrap();
            conn.execute(
                r#"
                INSERT INTO daemon_sessions (
                    session_id, name, state, rows, cols, pixel_width, pixel_height,
                    root_path, files_path, created_at_ms, updated_at_ms
                )
                VALUES (?1, 'new', 'running', 24, 80, 0, 0, '/home/me', NULL, 2000, 2000)
                "#,
                params![session_id_text(new_session_id)],
            )
            .unwrap();
        }

        let store = ClientHistoryStore::open(&state_path).unwrap();

        assert_eq!(
            store
                .list_sessions()
                .unwrap()
                .into_iter()
                .map(|record| (record.session_id, record.display_order))
                .collect::<Vec<_>>(),
            vec![(new_session_id, 0), (old_session_id, 1)]
        );

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn store_repairs_restored_session_metadata_without_overwriting_user_values() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-session-restore-store-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let missing_session_id = SessionId::new();
        let existing_session_id = SessionId::new();

        {
            let mut store = ClientHistoryStore::open(&state_path).unwrap();
            let repaired = store
                .record_session_restored(
                    missing_session_id,
                    SessionState::Running,
                    TerminalSize::new(32, 100),
                    "/tmp/restored-root",
                    "restored-shell",
                    "/tmp/restored-root",
                    UnixTimestampMillis(1_000),
                    UnixTimestampMillis(1_001),
                )
                .unwrap();

            assert_eq!(repaired.name.as_deref(), Some("restored-shell"));
            assert_eq!(repaired.root_path, "/tmp/restored-root");
            assert_eq!(repaired.files_path.as_deref(), Some("/tmp/restored-root"));

            store
                .record_session_created(
                    existing_session_id,
                    SessionState::Running,
                    TerminalSize::new(24, 80),
                    Some("initial shell"),
                    "/home/me",
                    UnixTimestampMillis(2_000),
                )
                .unwrap();
            store
                .record_session_renamed(
                    existing_session_id,
                    Some("kept name"),
                    UnixTimestampMillis(2_001),
                )
                .unwrap();
            store
                .record_session_files_path(
                    existing_session_id,
                    "/home/me/project",
                    UnixTimestampMillis(2_002),
                )
                .unwrap();

            let existing = store
                .record_session_restored(
                    existing_session_id,
                    SessionState::Running,
                    TerminalSize::new(40, 120),
                    "/tmp/default-root",
                    "default-name",
                    "/tmp/default-root",
                    UnixTimestampMillis(2_000),
                    UnixTimestampMillis(2_003),
                )
                .unwrap();

            assert_eq!(existing.name.as_deref(), Some("kept name"));
            assert_eq!(existing.root_path, "/home/me");
            assert_eq!(existing.files_path.as_deref(), Some("/home/me/project"));
            assert_eq!(existing.size, TerminalSize::new(40, 120));
        }

        let _ = fs::remove_file(sqlite_state_path_for_state_path(&state_path));
        let _ = fs::remove_file(state_path);
    }
}
