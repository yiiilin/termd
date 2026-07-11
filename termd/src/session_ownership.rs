use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, params};

use crate::pty::{CommandSpec, PtyBackend, PtyRestoreInfo, PtySize, PtyStartupGrant};

#[cfg(debug_assertions)]
pub(crate) fn test_crash_checkpoint(point: &str) {
    let Some(root) = std::env::var_os("TERMD_TEST_OWNERSHIP_CHECKPOINT_DIR").map(PathBuf::from)
    else {
        return;
    };
    if std::env::var("TERMD_TEST_OWNERSHIP_CHECKPOINT").as_deref() != Ok(point) {
        return;
    }
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    if root.parent() != Some(std::env::temp_dir().as_path())
        || !name.starts_with("termd-session-supervisor-test-")
    {
        return;
    }
    let reached = root.join(format!("{point}.reached"));
    let resume = root.join(format!("{point}.continue"));
    if std::fs::write(&reached, b"reached").is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !resume.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn test_crash_checkpoint(_point: &str) {}

const OWNERSHIP_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_ownership (
    session_id TEXT PRIMARY KEY NOT NULL,
    phase TEXT NOT NULL,
    create_operation_id BLOB NOT NULL,
    close_operation_id TEXT,
    capability BLOB NOT NULL,
    expected_socket TEXT,
    supervisor_pid INTEGER,
    socket_path TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    diagnostic TEXT
    , legacy_protocol INTEGER NOT NULL DEFAULT 0
    , owner_generation BLOB
) STRICT;
";

#[derive(Debug)]
pub(crate) enum OwnershipError {
    State(crate::state::StateError),
    Sqlite(rusqlite::Error),
    Pty(crate::pty::PtyError),
    Runtime(crate::runtime::RuntimeError),
    Preparation,
}

impl Display for OwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(error) => write!(f, "ownership state error: {error}"),
            Self::Sqlite(error) => write!(f, "ownership database error: {error}"),
            Self::Pty(error) => write!(f, "ownership PTY error: {error}"),
            Self::Runtime(error) => write!(f, "ownership runtime error: {error}"),
            Self::Preparation => write!(f, "owned session preparation failed"),
        }
    }
}

impl std::error::Error for OwnershipError {}

impl From<crate::state::StateError> for OwnershipError {
    fn from(error: crate::state::StateError) -> Self {
        Self::State(error)
    }
}

impl From<rusqlite::Error> for OwnershipError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<crate::pty::PtyError> for OwnershipError {
    fn from(error: crate::pty::PtyError) -> Self {
        Self::Pty(error)
    }
}

impl From<crate::runtime::RuntimeError> for OwnershipError {
    fn from(error: crate::runtime::RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

fn open_ledger(path: &Path) -> Result<Connection, OwnershipError> {
    Ok(crate::state::open_private_state_connection(path)?)
}

pub(crate) struct SessionOwnership<B: PtyBackend> {
    sqlite_path: PathBuf,
    backend: Arc<B>,
    generation: [u8; 16],
    wake: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl<B: PtyBackend> SessionOwnership<B> {
    pub(crate) fn open(
        state_path: impl AsRef<Path>,
        backend: Arc<B>,
    ) -> Result<Self, OwnershipError>
    where
        B: 'static,
    {
        let state_path = state_path.as_ref();
        let state = crate::state::StateStore::load(state_path)?;
        crate::state::StateStore::save(state_path, &state)?;
        let sqlite_path = crate::state::sqlite_state_path_for_state_path(state_path);
        let conn = open_ledger(&sqlite_path)?;
        conn.execute_batch(OWNERSHIP_SCHEMA)?;
        ensure_legacy_protocol_column(&conn)?;
        ensure_owner_generation_column(&conn)?;
        let mut generation = [0_u8; 16];
        OsRng.fill_bytes(&mut generation);
        let (wake, receiver) = mpsc::channel();
        let worker = spawn_reconciler(
            sqlite_path.clone(),
            Arc::clone(&backend),
            generation,
            receiver,
        )?;
        Ok(Self {
            sqlite_path,
            backend,
            generation,
            wake: Some(wake),
            worker: Some(worker),
        })
    }

    pub(crate) fn create<T>(
        &self,
        runtime: &mut crate::runtime::SessionRuntime<B>,
        session_id: &str,
        command: CommandSpec,
        size: PtySize,
        prepare: impl FnOnce(&mut crate::runtime::SessionRuntime<B>) -> Result<T, OwnershipError>,
    ) -> Result<T, OwnershipError> {
        let mut create_operation_id = [0_u8; 16];
        let mut capability = [0_u8; 32];
        OsRng.fill_bytes(&mut create_operation_id);
        OsRng.fill_bytes(&mut capability);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        let expected_socket = self.backend.expected_socket_path(session_id)?;
        let conn = open_ledger(&self.sqlite_path)?;
        conn.busy_timeout(Duration::ZERO)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let inserted = conn.execute(
            "INSERT INTO session_ownership (
                session_id, phase, create_operation_id, capability, expected_socket,
                created_at_ms, updated_at_ms, owner_generation
             ) VALUES (?1, 'preparing', ?2, ?3, ?4, ?5, ?5, ?6)",
            params![
                session_id,
                create_operation_id.as_slice(),
                capability.as_slice(),
                expected_socket.as_ref().map(|path| path.to_string_lossy()),
                now_ms,
                self.generation.as_slice(),
            ],
        );
        match inserted {
            Ok(_) => {
                test_crash_checkpoint("before_preparing_commit");
                conn.execute_batch("COMMIT")?;
                test_crash_checkpoint("after_preparing_commit");
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
        }
        let grant = PtyStartupGrant::new(create_operation_id, capability);
        let mut persist_evidence = |restore_info: &PtyRestoreInfo| {
            test_crash_checkpoint("after_spawn_before_evidence");
            self.record_prepared(session_id, restore_info)
                .map_err(crate::pty::PtyError::backend)?;
            test_crash_checkpoint("after_evidence_before_grant");
            Ok(())
        };
        let spawned = self.backend.spawn_named_gated(
            session_id,
            &command,
            size,
            &grant,
            &mut persist_evidence,
        );
        match spawned {
            Ok(session) => {
                let Some(restore_info) = session.restore_info() else {
                    self.fail_pre_active(runtime, session_id);
                    return Err(crate::pty::PtyError::Backend(
                        "persistent ownership create requires restore evidence".to_owned(),
                    )
                    .into());
                };
                runtime.publish_owned_session(
                    session_id,
                    session,
                    crate::session::TerminalSize {
                        rows: size.rows,
                        cols: size.cols,
                        pixel_width: size.pixel_width,
                        pixel_height: size.pixel_height,
                    },
                );
                let prepared = match prepare(runtime) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.fail_pre_active(runtime, session_id);
                        return Err(error);
                    }
                };
                test_crash_checkpoint("before_active_commit");
                if let Err(error) = self.commit_active(session_id, size, &restore_info) {
                    self.fail_pre_active(runtime, session_id);
                    return Err(error);
                }
                test_crash_checkpoint("after_active_commit");
                Ok(prepared)
            }
            Err(error) => {
                let _ = self.mark_cleaning(session_id);
                self.wake_reconciler();
                Err(error.into())
            }
        }
    }

    fn fail_pre_active(&self, runtime: &mut crate::runtime::SessionRuntime<B>, session_id: &str) {
        let _ = runtime.take_for_cleanup(session_id);
        let _ = self.mark_cleaning(session_id);
        self.wake_reconciler();
    }

    pub(crate) fn recover(
        &self,
        runtime: &mut crate::runtime::SessionRuntime<B>,
        records: Vec<crate::state::SessionStateRecord>,
    ) -> Result<Vec<crate::state::SessionStateRecord>, OwnershipError> {
        self.backfill_legacy_records(&records)?;
        let mut active = Vec::new();
        for record in records {
            let session_id = record.session_id.0.to_string();
            let conn = open_ledger(&self.sqlite_path)?;
            let ownership = conn.query_row(
                "SELECT phase, diagnostic FROM session_ownership WHERE session_id = ?1",
                [&session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            );
            let (phase, diagnostic) = match ownership {
                Ok(value) => value,
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(error) => return Err(error.into()),
            };
            if phase == "active" {
                if self.active_evidence_matches(&session_id, &record)? {
                    match reconnect_active_with_retry(runtime, &record) {
                        Ok(()) => active.push(record),
                        Err(error) => {
                            self.quarantine(
                                &session_id,
                                "owned supervisor could not be authenticated and reconnected",
                            )?;
                            tracing::warn!(%error, session_id, "owned supervisor recovery quarantined");
                        }
                    }
                } else {
                    self.quarantine(
                        &session_id,
                        "public runtime projection does not match durable ownership evidence",
                    )?;
                }
                continue;
            }
            if phase != "prepared" || diagnostic.as_deref() != Some("legacy_migration") {
                continue;
            }
            let Some(restore_info) = record.restore_info.as_ref() else {
                self.quarantine(&session_id, "legacy runtime row has no restore evidence")?;
                continue;
            };
            let capability: Vec<u8> = conn.query_row(
                "SELECT capability FROM session_ownership WHERE session_id = ?1",
                [&session_id],
                |row| row.get(0),
            )?;
            drop(conn);
            match self.backend.install_legacy_cleanup_capability(
                &session_id,
                restore_info,
                &capability,
            ) {
                Ok(installed) => {
                    let closing = matches!(
                        restore_info,
                        PtyRestoreInfo::UnixSocket {
                            supervisor_status: crate::pty::PtySupervisorStatus::Closing,
                            ..
                        }
                    );
                    let conn = open_ledger(&self.sqlite_path)?;
                    conn.execute(
                        "UPDATE session_ownership
                         SET phase = ?1, legacy_protocol = ?2, diagnostic = NULL,
                             close_operation_id = CASE WHEN ?1 = 'cleaning'
                                 THEN COALESCE(close_operation_id, ?3) ELSE close_operation_id END,
                             updated_at_ms = ?4
                         WHERE session_id = ?5 AND phase = 'prepared'",
                        params![
                            if closing { "cleaning" } else { "active" },
                            i64::from(!installed),
                            random_operation_id().to_string(),
                            unix_timestamp_millis(),
                            session_id,
                        ],
                    )?;
                    if closing {
                        test_crash_checkpoint("after_legacy_cleaning_commit");
                        self.wake_reconciler();
                    } else {
                        match reconnect_active_with_retry(runtime, &record) {
                            Ok(()) => active.push(record),
                            Err(error) => {
                                self.quarantine(
                                    &session_id,
                                    "migrated supervisor could not be authenticated and reconnected",
                                )?;
                                tracing::warn!(%error, session_id, "migrated supervisor recovery quarantined");
                            }
                        }
                    }
                }
                Err(error) => {
                    self.quarantine(&session_id, "legacy supervisor authority is unverified")?;
                    tracing::warn!(%error, session_id, "legacy supervisor migration quarantined");
                }
            }
        }
        Ok(active)
    }

    fn backfill_legacy_records(
        &self,
        records: &[crate::state::SessionStateRecord],
    ) -> Result<(), OwnershipError> {
        let conn = open_ledger(&self.sqlite_path)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<()> {
            for record in records {
                let session_id = record.session_id.0.to_string();
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM session_ownership WHERE session_id = ?1",
                    [&session_id],
                    |row| row.get(0),
                )?;
                if exists != 0 {
                    continue;
                }
                let mut create_operation = [0_u8; 16];
                let mut capability = [0_u8; 32];
                OsRng.fill_bytes(&mut create_operation);
                OsRng.fill_bytes(&mut capability);
                match record.restore_info.as_ref() {
                    Some(PtyRestoreInfo::UnixSocket {
                        socket_path,
                        supervisor_pid,
                        supervisor_status,
                    }) if record.state == termd_proto::SessionState::Running
                        && matches!(
                            supervisor_status,
                            crate::pty::PtySupervisorStatus::Running
                                | crate::pty::PtySupervisorStatus::Closing
                        )
                        && *supervisor_pid != 0
                        && !socket_path.as_os_str().is_empty() =>
                    {
                        conn.execute(
                            "INSERT INTO session_ownership (
                                session_id, phase, create_operation_id, capability,
                                expected_socket, supervisor_pid, socket_path,
                                created_at_ms, updated_at_ms, diagnostic
                             ) VALUES (?1, 'prepared', ?2, ?3, ?4, ?5, ?4, ?6, ?6,
                                       'legacy_migration')",
                            params![
                                session_id,
                                create_operation.as_slice(),
                                capability.as_slice(),
                                socket_path.to_string_lossy(),
                                i64::from(*supervisor_pid),
                                unix_timestamp_millis(),
                            ],
                        )?;
                    }
                    _ => {
                        conn.execute(
                            "INSERT INTO session_ownership (
                                session_id, phase, create_operation_id, capability,
                                created_at_ms, updated_at_ms, diagnostic
                             ) VALUES (?1, 'quarantined', ?2, ?3, ?4, ?4,
                                       'legacy runtime evidence is missing or invalid')",
                            params![
                                session_id,
                                create_operation.as_slice(),
                                capability.as_slice(),
                                unix_timestamp_millis(),
                            ],
                        )?;
                        conn.execute(
                            "DELETE FROM runtime_sessions WHERE session_id = ?1",
                            [session_id],
                        )?;
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn quarantine(&self, session_id: &str, diagnostic: &str) -> Result<(), OwnershipError> {
        let conn = open_ledger(&self.sqlite_path)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute(
                "UPDATE session_ownership SET phase = 'quarantined', diagnostic = ?1,
                 updated_at_ms = ?2 WHERE session_id = ?3",
                params![diagnostic, unix_timestamp_millis(), session_id],
            )?;
            conn.execute(
                "DELETE FROM runtime_sessions WHERE session_id = ?1",
                [session_id],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn active_evidence_matches(
        &self,
        session_id: &str,
        record: &crate::state::SessionStateRecord,
    ) -> Result<bool, OwnershipError> {
        let Some(PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            supervisor_status: crate::pty::PtySupervisorStatus::Running,
        }) = record.restore_info.as_ref()
        else {
            return Ok(false);
        };
        let conn = open_ledger(&self.sqlite_path)?;
        let durable = conn.query_row(
            "SELECT socket_path, supervisor_pid FROM session_ownership
             WHERE session_id = ?1 AND phase = 'active'",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        )?;
        Ok(
            durable.0.as_deref() == Some(socket_path.to_string_lossy().as_ref())
                && durable.1 == Some(i64::from(*supervisor_pid)),
        )
    }

    pub(crate) fn close(
        &self,
        runtime: &mut crate::runtime::SessionRuntime<B>,
        session_id: &str,
    ) -> Result<u64, OwnershipError> {
        let mut bytes = [0_u8; 8];
        OsRng.fill_bytes(&mut bytes);
        let proposed = u64::from_be_bytes(bytes).max(1);
        let conn = open_ledger(&self.sqlite_path)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<u64> {
            conn.execute(
                "UPDATE session_ownership
                 SET phase = 'cleaning',
                     close_operation_id = COALESCE(close_operation_id, ?1),
                     updated_at_ms = ?2
                 WHERE session_id = ?3 AND phase IN ('active', 'cleaning')",
                params![proposed.to_string(), unix_timestamp_millis(), session_id],
            )?;
            conn.query_row(
                "SELECT close_operation_id FROM session_ownership
                 WHERE session_id = ?1 AND phase = 'cleaning'",
                [session_id],
                |row| {
                    let value: String = row.get(0)?;
                    value.parse::<u64>().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
        })();
        match result {
            Ok(operation_id) => {
                conn.execute_batch("COMMIT")?;
                match runtime.take_for_cleanup(session_id) {
                    Ok(cleanup) => drop(cleanup),
                    Err(crate::runtime::RuntimeError::SessionNotFound) => {}
                    Err(error) => return Err(error.into()),
                }
                self.wake_reconciler();
                let deadline = std::time::Instant::now() + Duration::from_secs(12);
                while std::time::Instant::now() < deadline {
                    let conn = open_ledger(&self.sqlite_path)?;
                    let remaining: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM session_ownership WHERE session_id = ?1",
                        [session_id],
                        |row| row.get(0),
                    )?;
                    if remaining == 0 {
                        return Ok(operation_id);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(crate::pty::PtyError::Backend(
                    "timed out waiting for owned session cleanup".to_owned(),
                )
                .into())
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error.into())
            }
        }
    }

    fn wake_reconciler(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }

    fn record_prepared(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
    ) -> Result<(), OwnershipError> {
        let PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            ..
        } = restore_info
        else {
            return Err(crate::pty::PtyError::Backend(
                "gated supervisor spawn returned non-Unix restore evidence".to_owned(),
            )
            .into());
        };
        let conn = open_ledger(&self.sqlite_path)?;
        let updated = conn.execute(
            "UPDATE session_ownership
             SET phase = 'prepared', supervisor_pid = ?1, socket_path = ?2, updated_at_ms = ?3
             WHERE session_id = ?4 AND phase = 'preparing'
               AND (expected_socket IS NULL OR expected_socket = ?2)",
            params![
                i64::from(*supervisor_pid),
                socket_path.to_string_lossy(),
                unix_timestamp_millis(),
                session_id
            ],
        )?;
        if updated != 1 {
            return Err(crate::pty::PtyError::Backend(
                "supervisor evidence did not match durable preparing intent".to_owned(),
            )
            .into());
        }
        Ok(())
    }

    fn mark_cleaning(&self, session_id: &str) -> Result<(), OwnershipError> {
        let conn = open_ledger(&self.sqlite_path)?;
        conn.execute(
            "UPDATE session_ownership SET phase = 'cleaning', updated_at_ms = ?1
             WHERE session_id = ?2 AND phase != 'active'",
            params![unix_timestamp_millis(), session_id],
        )?;
        Ok(())
    }

    fn commit_active(
        &self,
        session_id: &str,
        size: PtySize,
        restore_info: &PtyRestoreInfo,
    ) -> Result<(), OwnershipError> {
        let (restore_kind, restore_value) =
            crate::state::serialize_restore_info(Some(restore_info));
        let now_ms = unix_timestamp_millis();
        let conn = open_ledger(&self.sqlite_path)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<()> {
            let active = conn.execute(
                "UPDATE session_ownership SET phase = 'active', updated_at_ms = ?1
                 WHERE session_id = ?2 AND phase = 'prepared'",
                params![now_ms, session_id],
            )?;
            if active != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            conn.execute(
                "INSERT INTO runtime_sessions (
                    session_id, state, rows, cols, pixel_width, pixel_height,
                    created_at_ms, updated_at_ms, restore_kind, restore_value
                 ) VALUES (?1, 'running', ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
                params![
                    session_id,
                    i64::from(size.rows),
                    i64::from(size.cols),
                    i64::from(size.pixel_width),
                    i64::from(size.pixel_height),
                    now_ms,
                    restore_kind,
                    restore_value,
                ],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                let _ = self.mark_cleaning(session_id);
                return Err(error.into());
            }
        }
        Ok(())
    }
}

struct CleanupRow {
    session_id: String,
    phase: String,
    capability: Vec<u8>,
    socket_path: PathBuf,
    supervisor_pid: u32,
    operation_id: Option<u64>,
    legacy_protocol: bool,
}

fn spawn_reconciler<B: PtyBackend + 'static>(
    sqlite_path: PathBuf,
    backend: Arc<B>,
    generation: [u8; 16],
    receiver: mpsc::Receiver<()>,
) -> Result<JoinHandle<()>, OwnershipError> {
    thread::Builder::new()
        .name("termd-session-ownership".to_owned())
        .spawn(move || {
            loop {
                if let Err(error) = reconcile_once(&sqlite_path, backend.as_ref(), &generation) {
                    tracing::warn!(%error, "session ownership reconciliation failed");
                }
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|error| OwnershipError::Pty(crate::pty::PtyError::from(error)))
}

fn reconcile_once<B: PtyBackend>(
    sqlite_path: &Path,
    backend: &B,
    generation: &[u8; 16],
) -> Result<(), OwnershipError> {
    reconcile_interrupted_creates(sqlite_path, generation)?;
    let conn = open_ledger(sqlite_path)?;
    let mut statement = conn.prepare(
        "SELECT session_id, phase, capability, socket_path, supervisor_pid, close_operation_id,
                legacy_protocol
         FROM session_ownership
         WHERE phase IN ('active', 'cleaning')
           AND socket_path IS NOT NULL AND supervisor_pid IS NOT NULL
         ORDER BY created_at_ms, session_id",
    )?;
    let rows = statement.query_map([], |row| {
        let operation_id = row
            .get::<_, Option<String>>(5)?
            .map(|value| {
                value.parse::<u64>().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?;
        let pid = u32::try_from(row.get::<_, i64>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
        Ok(CleanupRow {
            session_id: row.get(0)?,
            phase: row.get(1)?,
            capability: row.get(2)?,
            socket_path: PathBuf::from(row.get::<_, String>(3)?),
            supervisor_pid: pid,
            operation_id,
            legacy_protocol: row.get::<_, i64>(6)? != 0,
        })
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    drop(conn);

    for row in rows {
        let running_restore = PtyRestoreInfo::UnixSocket {
            socket_path: row.socket_path.clone(),
            supervisor_pid: row.supervisor_pid,
            supervisor_status: crate::pty::PtySupervisorStatus::Running,
        };
        if row.phase == "active" {
            if backend
                .owned_natural_exit_status(&running_restore)
                .unwrap_or(false)
            {
                mark_cleaning_with_stable_operation(sqlite_path, &row.session_id)?;
            }
            continue;
        }
        let operation_id = match row.operation_id {
            Some(operation_id) => operation_id,
            None => mark_cleaning_with_stable_operation(sqlite_path, &row.session_id)?,
        };
        let closing_restore = PtyRestoreInfo::UnixSocket {
            socket_path: row.socket_path.clone(),
            supervisor_pid: row.supervisor_pid,
            supervisor_status: crate::pty::PtySupervisorStatus::Closing,
        };
        let cleanup = if row.legacy_protocol {
            backend.reconcile_legacy_owned_cleanup(&row.session_id, &closing_restore)
        } else {
            backend.reconcile_owned_cleanup(
                &row.session_id,
                &closing_restore,
                &row.capability,
                operation_id,
            )
        };
        match cleanup {
            Ok(true) => finalize_closed(sqlite_path, &row.session_id)?,
            Ok(false) if supervisor_pid_confirmed_absent(row.supervisor_pid) => {
                finalize_closed(sqlite_path, &row.session_id)?;
            }
            Ok(false) => {}
            Err(_error) if supervisor_pid_confirmed_absent(row.supervisor_pid) => {
                finalize_closed(sqlite_path, &row.session_id)?;
            }
            Err(error) => {
                tracing::warn!(%error, session_id = %row.session_id, "owned supervisor cleanup remains pending");
            }
        }
    }
    Ok(())
}

fn reconcile_interrupted_creates(
    sqlite_path: &Path,
    generation: &[u8; 16],
) -> Result<(), OwnershipError> {
    let conn = open_ledger(sqlite_path)?;
    let mut statement = conn.prepare(
        "SELECT session_id FROM session_ownership
         WHERE phase = 'prepared' AND diagnostic IS NULL
           AND (owner_generation IS NULL OR owner_generation != ?1)",
    )?;
    let prepared = statement
        .query_map([generation.as_slice()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for session_id in prepared {
        mark_cleaning_with_stable_operation(sqlite_path, &session_id)?;
    }

    let mut statement = conn.prepare(
        "SELECT session_id, expected_socket FROM session_ownership
         WHERE phase = 'preparing'
           AND (owner_generation IS NULL OR owner_generation != ?1)",
    )?;
    let preparing = statement
        .query_map([generation.as_slice()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (session_id, expected_socket) in preparing {
        let socket_absent = expected_socket
            .as_deref()
            .is_none_or(|path| !Path::new(path).exists());
        if socket_absent {
            conn.execute(
                "DELETE FROM session_ownership
                 WHERE session_id = ?1 AND phase = 'preparing'",
                [session_id],
            )?;
        }
    }
    Ok(())
}

fn mark_cleaning_with_stable_operation(
    sqlite_path: &Path,
    session_id: &str,
) -> Result<u64, OwnershipError> {
    let mut bytes = [0_u8; 8];
    OsRng.fill_bytes(&mut bytes);
    let proposed = u64::from_be_bytes(bytes).max(1);
    let conn = open_ledger(sqlite_path)?;
    conn.execute(
        "UPDATE session_ownership
         SET phase = 'cleaning', close_operation_id = COALESCE(close_operation_id, ?1),
             updated_at_ms = ?2
         WHERE session_id = ?3 AND phase IN ('prepared', 'active', 'cleaning')",
        params![proposed.to_string(), unix_timestamp_millis(), session_id],
    )?;
    let value: String = conn.query_row(
        "SELECT close_operation_id FROM session_ownership WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    value.parse::<u64>().map_err(|error| {
        OwnershipError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(error),
        ))
    })
}

fn finalize_closed(sqlite_path: &Path, session_id: &str) -> Result<(), OwnershipError> {
    let conn = open_ledger(sqlite_path)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE runtime_sessions
             SET state = 'closed', restore_kind = NULL, restore_value = NULL,
                 updated_at_ms = ?1
             WHERE session_id = ?2 AND state != 'closed'",
            params![unix_timestamp_millis(), session_id],
        )?;
        conn.execute(
            "DELETE FROM runtime_sessions WHERE session_id = ?1 AND state = 'closed'",
            [session_id],
        )?;
        conn.execute(
            "DELETE FROM session_ownership WHERE session_id = ?1 AND phase = 'cleaning'",
            [session_id],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn supervisor_pid_confirmed_absent(pid: u32) -> bool {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    if !proc_path.exists() {
        return true;
    }
    std::fs::read_to_string(proc_path.join("stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, fields)| fields.starts_with('Z'))
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn supervisor_pid_confirmed_absent(_pid: u32) -> bool {
    false
}

impl<B: PtyBackend> SessionOwnership<B> {
    pub(crate) fn shutdown(&mut self) {
        self.wake.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn reconnect_active_with_retry<B: PtyBackend>(
    runtime: &mut crate::runtime::SessionRuntime<B>,
    record: &crate::state::SessionStateRecord,
) -> crate::runtime::RuntimeResult<()> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        match runtime.reconnect_session(record) {
            Ok(()) => return Ok(()),
            Err(crate::runtime::RuntimeError::Pty(error)) if attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    %error,
                    session_id = %record.session_id.0,
                    attempt,
                    "owned supervisor reconnect failed transiently; retrying"
                );
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded reconnect loop always returns")
}

fn unix_timestamp_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn random_operation_id() -> u64 {
    let mut bytes = [0_u8; 8];
    OsRng.fill_bytes(&mut bytes);
    u64::from_be_bytes(bytes).max(1)
}

fn ensure_legacy_protocol_column(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare("PRAGMA table_info(session_ownership)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if !columns.iter().any(|column| column == "legacy_protocol") {
        conn.execute(
            "ALTER TABLE session_ownership
             ADD COLUMN legacy_protocol INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn ensure_owner_generation_column(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare("PRAGMA table_info(session_ownership)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if !columns.iter().any(|column| column == "owner_generation") {
        conn.execute(
            "ALTER TABLE session_ownership ADD COLUMN owner_generation BLOB",
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rusqlite::{Connection, params};

    use crate::pty::{
        CommandSpec, PtyBackend, PtyError, PtyExitStatus, PtyRestoreInfo, PtyResult, PtySession,
        PtySize, PtySnapshot, PtyStartupGrant,
    };

    struct CountingBackend {
        spawns: Arc<AtomicUsize>,
    }

    struct GatedBackend {
        sqlite_path: PathBuf,
        socket_path: PathBuf,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    struct NoopSession {
        restore_info: PtyRestoreInfo,
    }

    impl PtySession for NoopSession {
        fn read(&mut self, _buffer: &mut [u8]) -> PtyResult<usize> {
            Ok(0)
        }
        fn write_all(&mut self, _bytes: &[u8]) -> PtyResult<()> {
            Ok(())
        }
        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }
        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::new(24, 80),
                process_id: Some(4242),
                retained_output: Vec::new(),
            })
        }
        fn restore_info(&self) -> Option<PtyRestoreInfo> {
            Some(self.restore_info.clone())
        }
        fn terminate(&mut self) -> PtyResult<()> {
            Ok(())
        }
        fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> PtyResult<PtyExitStatus> {
            Ok(PtyExitStatus::exited(0))
        }
        fn process_id(&self) -> Option<u32> {
            Some(4242)
        }
    }

    struct SuccessfulGatedBackend {
        socket_path: PathBuf,
    }

    struct SlowPreparedBackend {
        socket_path: PathBuf,
        sqlite_path: PathBuf,
    }

    struct TransientReconnectBackend {
        socket_path: PathBuf,
        reconnect_attempts: AtomicUsize,
    }

    #[derive(Default)]
    struct ReconcilingBackend {
        cleanup_calls: AtomicUsize,
    }

    impl PtyBackend for SuccessfulGatedBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            panic!("ownership create must use gated spawn")
        }
        fn expected_socket_path(&self, _session_id: &str) -> PtyResult<Option<PathBuf>> {
            Ok(Some(self.socket_path.clone()))
        }
        fn spawn_named_gated(
            &self,
            _session_id: &str,
            _command: &CommandSpec,
            _size: PtySize,
            _grant: &PtyStartupGrant,
            evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
        ) -> PtyResult<Box<dyn PtySession>> {
            let restore_info = PtyRestoreInfo::UnixSocket {
                socket_path: self.socket_path.clone(),
                supervisor_pid: 4242,
                supervisor_status: crate::pty::PtySupervisorStatus::Running,
            };
            evidence_committed(&restore_info)?;
            Ok(Box::new(NoopSession { restore_info }))
        }

        fn reconnect(
            &self,
            _session_id: &str,
            restore_info: &PtyRestoreInfo,
            _size: PtySize,
        ) -> PtyResult<Box<dyn PtySession>> {
            Ok(Box::new(NoopSession {
                restore_info: restore_info.clone(),
            }))
        }
    }

    impl PtyBackend for SlowPreparedBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            panic!("ownership create must use gated spawn")
        }

        fn expected_socket_path(&self, _session_id: &str) -> PtyResult<Option<PathBuf>> {
            Ok(Some(self.socket_path.clone()))
        }

        fn spawn_named_gated(
            &self,
            session_id: &str,
            _command: &CommandSpec,
            _size: PtySize,
            _grant: &PtyStartupGrant,
            evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
        ) -> PtyResult<Box<dyn PtySession>> {
            let restore_info = PtyRestoreInfo::UnixSocket {
                socket_path: self.socket_path.clone(),
                supervisor_pid: 4243,
                supervisor_status: crate::pty::PtySupervisorStatus::Running,
            };
            evidence_committed(&restore_info)?;
            std::thread::sleep(Duration::from_millis(400));
            let phase: String = Connection::open(&self.sqlite_path)
                .unwrap()
                .query_row(
                    "SELECT phase FROM session_ownership WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(phase, "prepared");
            Ok(Box::new(NoopSession { restore_info }))
        }
    }

    impl PtyBackend for TransientReconnectBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            panic!("ownership create must use gated spawn")
        }

        fn expected_socket_path(&self, _session_id: &str) -> PtyResult<Option<PathBuf>> {
            Ok(Some(self.socket_path.clone()))
        }

        fn spawn_named_gated(
            &self,
            _session_id: &str,
            _command: &CommandSpec,
            _size: PtySize,
            _grant: &PtyStartupGrant,
            evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
        ) -> PtyResult<Box<dyn PtySession>> {
            let restore_info = PtyRestoreInfo::UnixSocket {
                socket_path: self.socket_path.clone(),
                supervisor_pid: 4244,
                supervisor_status: crate::pty::PtySupervisorStatus::Running,
            };
            evidence_committed(&restore_info)?;
            Ok(Box::new(NoopSession { restore_info }))
        }

        fn reconnect(
            &self,
            _session_id: &str,
            restore_info: &PtyRestoreInfo,
            _size: PtySize,
        ) -> PtyResult<Box<dyn PtySession>> {
            if self.reconnect_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(PtyError::Backend(
                    "injected transient reconnect failure".to_owned(),
                ));
            }
            Ok(Box::new(NoopSession {
                restore_info: restore_info.clone(),
            }))
        }
    }

    impl PtyBackend for ReconcilingBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            panic!("test backend does not spawn")
        }

        fn reconcile_owned_cleanup(
            &self,
            _session_id: &str,
            _restore_info: &PtyRestoreInfo,
            _capability: &[u8],
            _operation_id: u64,
        ) -> PtyResult<bool> {
            self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    impl PtyBackend for GatedBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            panic!("ownership create must use gated spawn")
        }

        fn expected_socket_path(&self, _session_id: &str) -> PtyResult<Option<PathBuf>> {
            Ok(Some(self.socket_path.clone()))
        }

        fn spawn_named_gated(
            &self,
            session_id: &str,
            _command: &CommandSpec,
            _size: PtySize,
            grant: &PtyStartupGrant,
            evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
        ) -> PtyResult<Box<dyn PtySession>> {
            self.events.lock().unwrap().push("spawn");
            let evidence = PtyRestoreInfo::UnixSocket {
                socket_path: self.socket_path.clone(),
                supervisor_pid: 4242,
                supervisor_status: crate::pty::PtySupervisorStatus::Running,
            };
            evidence_committed(&evidence)?;
            let phase: String = Connection::open(&self.sqlite_path)
                .unwrap()
                .query_row(
                    "SELECT phase FROM session_ownership WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(phase, "prepared");
            self.events.lock().unwrap().push("prepared");
            assert!(!grant.capability().is_empty());
            self.events.lock().unwrap().push("grant");
            Err(PtyError::Backend("stop after grant observation".to_owned()))
        }
    }

    impl PtyBackend for CountingBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Err(PtyError::Backend("spawn should not run".to_owned()))
        }
    }

    #[test]
    fn create_does_not_spawn_when_preparing_intent_cannot_commit() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-intent-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        crate::state::StateStore::load(&state_path).unwrap();
        let sqlite_path = state_path.with_extension("sqlite");
        let spawns = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(CountingBackend {
            spawns: Arc::clone(&spawns),
        });
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);
        let lock = Connection::open(&sqlite_path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let result = ownership.create(
            &mut runtime,
            "00000000-0000-0000-0000-000000000001",
            CommandSpec::new("sh"),
            PtySize::new(24, 80),
            |_| Ok(()),
        );

        assert!(result.is_err());
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
        drop(lock);
        let _ = std::fs::remove_file(sqlite_path);
    }

    #[test]
    fn create_commits_pid_evidence_before_startup_grant() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-grant-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        crate::state::StateStore::load(&state_path).unwrap();
        let sqlite_path = state_path.with_extension("sqlite");
        let socket_path = state_path.with_extension("sock");
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(GatedBackend {
            sqlite_path: sqlite_path.clone(),
            socket_path,
            events: Arc::clone(&events),
        });
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);

        let result = ownership.create(
            &mut runtime,
            "00000000-0000-0000-0000-000000000002",
            CommandSpec::new("sh"),
            PtySize::new(24, 80),
            |_| Ok(()),
        );

        assert!(result.is_err());
        assert_eq!(&*events.lock().unwrap(), &["spawn", "prepared", "grant"]);
        let _ = std::fs::remove_file(sqlite_path);
    }

    #[test]
    fn successful_create_atomically_publishes_active_runtime_projection() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-active-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let backend = Arc::new(SuccessfulGatedBackend {
            socket_path: state_path.with_extension("sock"),
        });
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);
        let session_id = "00000000-0000-0000-0000-000000000003";

        ownership
            .create(
                &mut runtime,
                session_id,
                CommandSpec::new("sh"),
                PtySize::new(24, 80),
                |_| Ok(()),
            )
            .unwrap();
        let conn = Connection::open(&sqlite_path).unwrap();
        let (phase, public_state): (String, String) = conn
            .query_row(
                "SELECT o.phase, r.state
                 FROM session_ownership o JOIN runtime_sessions r USING (session_id)
                 WHERE o.session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(phase, "active");
        assert_eq!(public_state, "running");
        let public_columns = conn
            .prepare("PRAGMA table_info(runtime_sessions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!public_columns.iter().any(|column| {
            matches!(column.as_str(), "cleanup_capability" | "close_operation_id")
        }));
        let _ = std::fs::remove_file(sqlite_path);
    }

    #[test]
    fn reconciler_never_claims_current_generation_prepared_create() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-generation-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let backend = Arc::new(SlowPreparedBackend {
            socket_path: state_path.with_extension("sock"),
            sqlite_path: sqlite_path.clone(),
        });
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);

        ownership
            .create(
                &mut runtime,
                "00000000-0000-0000-0000-000000000004",
                CommandSpec::new("sh"),
                PtySize::new(24, 80),
                |_| Ok(()),
            )
            .unwrap();

        let phase: String = Connection::open(&sqlite_path)
            .unwrap()
            .query_row(
                "SELECT phase FROM session_ownership WHERE session_id = ?1",
                ["00000000-0000-0000-0000-000000000004"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "active");
    }

    #[test]
    fn previous_generation_prepared_create_is_hidden_and_reconciled() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-interrupted-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let backend = Arc::new(ReconcilingBackend::default());
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let session_id = "00000000-0000-0000-0000-000000000005";
        Connection::open(&sqlite_path)
            .unwrap()
            .execute(
                "INSERT INTO session_ownership (
                    session_id, phase, create_operation_id, capability, expected_socket,
                    supervisor_pid, socket_path, created_at_ms, updated_at_ms, owner_generation
                 ) VALUES (?1, 'prepared', ?2, ?3, ?4, ?5, ?4, 1, 1, ?6)",
                params![
                    session_id,
                    [1_u8; 16].as_slice(),
                    [2_u8; 32].as_slice(),
                    state_path.with_extension("sock").to_string_lossy(),
                    i64::from(u32::MAX),
                    [9_u8; 16].as_slice(),
                ],
            )
            .unwrap();
        ownership.wake_reconciler();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let count: i64 = Connection::open(&sqlite_path)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM session_ownership WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap();
            if count == 0 {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(backend.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(
            crate::state::StateStore::load(&state_path)
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[test]
    fn active_projection_with_mismatched_evidence_is_quarantined() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-quarantine-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let session_id = "00000000-0000-0000-0000-000000000006";
        let backend = Arc::new(SuccessfulGatedBackend {
            socket_path: state_path.with_extension("sock"),
        });
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);
        ownership
            .create(
                &mut runtime,
                session_id,
                CommandSpec::new("sh"),
                PtySize::new(24, 80),
                |_| Ok(()),
            )
            .unwrap();
        Connection::open(&sqlite_path)
            .unwrap()
            .execute(
                "UPDATE session_ownership SET supervisor_pid = supervisor_pid + 1
                 WHERE session_id = ?1",
                [session_id],
            )
            .unwrap();

        let recovered = ownership
            .recover(
                &mut runtime,
                crate::state::StateStore::load(&state_path)
                    .unwrap()
                    .sessions,
            )
            .unwrap();
        let phase: String = Connection::open(&sqlite_path)
            .unwrap()
            .query_row(
                "SELECT phase FROM session_ownership WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recovered.is_empty());
        assert_eq!(phase, "quarantined");
    }

    #[test]
    fn active_recovery_retries_one_transient_pty_reconnect_failure() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-transient-reconnect-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let session_id = "00000000-0000-0000-0000-000000000009";
        let backend = Arc::new(TransientReconnectBackend {
            socket_path: state_path.with_extension("sock"),
            reconnect_attempts: AtomicUsize::new(0),
        });
        {
            let ownership =
                super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
            let mut runtime =
                crate::runtime::SessionRuntime::from_shared_backend(Arc::clone(&backend));
            ownership
                .create(
                    &mut runtime,
                    session_id,
                    CommandSpec::new("sh"),
                    PtySize::new(24, 80),
                    |_| Ok(()),
                )
                .unwrap();
        }

        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(Arc::clone(&backend));
        let recovered = ownership
            .recover(
                &mut runtime,
                crate::state::StateStore::load(&state_path)
                    .unwrap()
                    .sessions,
            )
            .unwrap();
        let phase: String = Connection::open(&sqlite_path)
            .unwrap()
            .query_row(
                "SELECT phase FROM session_ownership WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(backend.reconnect_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(recovered.len(), 1);
        assert_eq!(phase, "active");
    }

    #[test]
    fn active_commit_survives_lost_response_and_recovers_after_restart() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-response-loss-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let session_id = "00000000-0000-0000-0000-000000000007";
        let backend = Arc::new(SuccessfulGatedBackend {
            socket_path: state_path.with_extension("sock"),
        });
        {
            let ownership =
                super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
            let mut runtime =
                crate::runtime::SessionRuntime::from_shared_backend(Arc::clone(&backend));
            ownership
                .create(
                    &mut runtime,
                    session_id,
                    CommandSpec::new("sh"),
                    PtySize::new(24, 80),
                    |_| Ok(()),
                )
                .unwrap();
        }

        let ownership = super::SessionOwnership::open(&state_path, backend).unwrap();
        let backend = Arc::new(SuccessfulGatedBackend {
            socket_path: state_path.with_extension("sock"),
        });
        let mut runtime = crate::runtime::SessionRuntime::from_shared_backend(backend);
        let recovered = ownership
            .recover(
                &mut runtime,
                crate::state::StateStore::load(&state_path)
                    .unwrap()
                    .sessions,
            )
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].session_id.0.to_string(), session_id);
        assert_eq!(recovered[0].state, termd_proto::SessionState::Running);
    }

    #[test]
    fn locked_drop_and_failed_restart_keep_durable_cleanup_for_next_open() {
        let state_path = std::env::temp_dir().join(format!(
            "termd-ownership-locked-drop-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = state_path.with_extension("sqlite");
        let backend = Arc::new(ReconcilingBackend::default());
        let ownership = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let session_id = "00000000-0000-0000-0000-000000000008";
        let lock = Connection::open(&sqlite_path).unwrap();
        lock.execute(
            "INSERT INTO session_ownership (
                session_id, phase, create_operation_id, close_operation_id, capability,
                expected_socket, supervisor_pid, socket_path, created_at_ms, updated_at_ms
             ) VALUES (?1, 'cleaning', ?2, '17', ?3, ?4, ?5, ?4, 1, 1)",
            params![
                session_id,
                [1_u8; 16].as_slice(),
                [2_u8; 32].as_slice(),
                state_path.with_extension("sock").to_string_lossy(),
                i64::from(u32::MAX),
            ],
        )
        .unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let dropped_at = std::time::Instant::now();
        drop(ownership);
        assert!(dropped_at.elapsed() < Duration::from_secs(1));
        assert!(super::SessionOwnership::open(&state_path, Arc::clone(&backend)).is_err());
        let durable_count: i64 = lock
            .query_row(
                "SELECT COUNT(*) FROM session_ownership WHERE session_id = ?1 AND phase = 'cleaning'",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(durable_count, 1);
        lock.execute_batch("ROLLBACK").unwrap();

        let recovered = super::SessionOwnership::open(&state_path, Arc::clone(&backend)).unwrap();
        recovered.wake_reconciler();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining: i64 = Connection::open(&sqlite_path)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM session_ownership WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap();
            if remaining == 0 {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(backend.cleanup_calls.load(Ordering::SeqCst), 1);
    }
}
