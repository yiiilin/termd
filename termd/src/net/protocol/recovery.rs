use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::watch;

use super::*;

/// supervisor 恢复后补给协议层的可见 session 元数据。
#[derive(Debug, Clone)]
struct RestoredSessionMetadata {
    name: Option<String>,
    root_path: PathBuf,
}

impl<B, V> DaemonProtocol<B, V>
where
    B: PtyBackend,
    V: SignatureVerifier,
{
    pub(super) fn repair_visible_session_metadata_for(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), ProtocolError> {
        let current_record = self
            .client_history
            .session_record_including_closed(session_id)?;
        if matches!(
            current_record.as_ref().map(|record| record.state),
            Some(SessionState::Running | SessionState::Created)
        ) {
            return Ok(());
        }

        if let Some(internal_id) = self.session_index.get(&session_id).cloned() {
            let state = self.runtime_state_proto(&internal_id)?;
            let size = self.runtime_size_proto(&internal_id)?;
            let root_path = self
                .session_roots
                .get(&session_id)
                .cloned()
                .or_else(|| {
                    current_record
                        .as_ref()
                        .map(|record| PathBuf::from(&record.root_path))
                })
                .unwrap_or_else(|| self.default_restored_session_root());
            let files_path = current_record
                .as_ref()
                .and_then(|record| record.files_path.as_ref().map(PathBuf::from))
                .unwrap_or_else(|| root_path.clone());
            let default_name = self
                .session_names
                .get(&session_id)
                .cloned()
                .or_else(|| {
                    current_record
                        .as_ref()
                        .and_then(|record| record.name.clone())
                })
                .unwrap_or_else(|| default_restored_session_name(session_id));
            let created_at_ms = current_record
                .as_ref()
                .map(|record| record.created_at_ms)
                .unwrap_or_else(current_unix_timestamp_millis);
            self.client_history.record_session_restored(
                session_id,
                state,
                size,
                &root_path,
                &default_name,
                &files_path,
                created_at_ms,
                current_unix_timestamp_millis(),
            )?;
            return Ok(());
        }

        Ok(())
    }

    pub(super) fn restore_runtime_sessions(&mut self, sessions: Vec<SessionStateRecord>) {
        let persisted_by_id = self.visible_session_metadata_by_id();

        for session in sessions {
            let wire_session_id = session.session_id;
            // runtime_sessions 的 restore_info 是 supervisor 可重连事实；client history
            // 缺失只影响展示元数据，不能让存活 session 从 Web 列表消失。
            if session.state != SessionState::Running
                || session.restore_info.is_none()
                || !restore_info_is_reconnectable(session.restore_info.as_ref())
            {
                self.mark_persisted_session_closed(wire_session_id);
                continue;
            }

            match self.runtime.reconnect_session(&session) {
                Ok(()) => {
                    let metadata = self
                        .restored_session_metadata(&session, persisted_by_id.get(&wire_session_id));
                    self.register_restored_runtime_session(&session, metadata);
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        session_id = %wire_session_id.0,
                        "failed to reconnect persisted session supervisor; marking session closed"
                    );
                    // 中文注释：session 的运行事实只能来自 live supervisor。启动恢复已经给
                    // stale socket 一次重连机会；失败后必须关闭并移除可见状态，不能再把
                    // 它保留成 running，也不能让 session.list/attach 同步重试旧 socket。
                    self.mark_persisted_session_closed(wire_session_id);
                }
            }
        }
        if let Err(error) = self.persist_state() {
            tracing::warn!(%error, "failed to persist recovered session supervisor state");
        }
    }

    fn register_restored_runtime_session(
        &mut self,
        session: &SessionStateRecord,
        metadata: RestoredSessionMetadata,
    ) {
        let wire_session_id = session.session_id;
        let internal_session_id = wire_session_id.0.to_string();
        self.session_index
            .insert(wire_session_id, internal_session_id);
        self.session_output_history_mut(wire_session_id, session.size);
        let (file_tree_signal, _) = watch::channel(0);
        self.session_file_tree_signals
            .insert(wire_session_id, file_tree_signal);
        let (resize_signal, _) = watch::channel(session.size);
        self.session_resize_signals
            .insert(wire_session_id, resize_signal);
        self.session_roots
            .insert(wire_session_id, metadata.root_path);
        if let Some(name) = metadata.name {
            self.session_names.insert(wire_session_id, name);
        }
    }

    fn visible_session_metadata_by_id(&self) -> HashMap<SessionId, SessionHistoryRecord> {
        match self.restore_session_metadata_by_id() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to load session metadata while restoring supervisors"
                );
                HashMap::new()
            }
        }
    }

    fn restore_session_metadata_by_id(
        &self,
    ) -> Result<HashMap<SessionId, SessionHistoryRecord>, StateError> {
        let mut records = HashMap::new();
        for record in self.client_history.list_sessions()? {
            records.insert(record.session_id, record);
        }

        // snapshot_state 和 list_sessions 只看可见行；这里先收集当前仍可见的元数据。
        // 对已经注册进 `session_index` 的 runtime session，再额外补查 closed 行，避免
        // 运行中修复展示元数据时丢掉用户设置的 session 名称。启动恢复首轮如果还没注册
        // `session_index`，则由后面的 `restored_session_metadata()` 再按 session 回退补查。
        for session_id in self.session_index.keys() {
            if records.contains_key(session_id) {
                continue;
            }
            if let Some(record) = self
                .client_history
                .session_record_including_closed(*session_id)?
            {
                records.insert(*session_id, record);
            }
        }

        Ok(records)
    }

    fn restored_session_metadata(
        &mut self,
        session: &SessionStateRecord,
        persisted: Option<&SessionHistoryRecord>,
    ) -> RestoredSessionMetadata {
        if let Some(record) = persisted {
            return self.restore_session_metadata_from_existing_record(session, record);
        }
        match self
            .client_history
            .session_record_including_closed(session.session_id)
        {
            Ok(Some(record)) => {
                return self.restore_session_metadata_from_existing_record(session, &record);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    session_id = %session.session_id.0,
                    "failed to load closed session metadata while restoring supervisor"
                );
            }
        }

        let root_path = self.default_restored_session_root();
        let default_name = default_restored_session_name(session.session_id);

        match self.client_history.record_session_restored(
            session.session_id,
            session.state,
            session.size,
            &root_path,
            &default_name,
            &root_path,
            session.created_at_ms,
            session.updated_at_ms,
        ) {
            Ok(record) => restored_session_metadata_from_record(&record),
            Err(error) => {
                tracing::warn!(
                    %error,
                    session_id = %session.session_id.0,
                    "failed to repair restored session metadata in sqlite history"
                );
                // SQLite 元数据修复失败不能让已经重连成功的 supervisor 再次不可见。
                RestoredSessionMetadata {
                    name: Some(default_name),
                    root_path,
                }
            }
        }
    }

    fn restore_session_metadata_from_existing_record(
        &mut self,
        session: &SessionStateRecord,
        record: &SessionHistoryRecord,
    ) -> RestoredSessionMetadata {
        let root_path = PathBuf::from(&record.root_path);
        let files_path = record
            .files_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| root_path.clone());
        let default_name = record
            .name
            .clone()
            .unwrap_or_else(|| default_restored_session_name(session.session_id));

        match self.client_history.record_session_restored(
            session.session_id,
            session.state,
            session.size,
            &root_path,
            &default_name,
            &files_path,
            record.created_at_ms,
            session.updated_at_ms,
        ) {
            Ok(repaired) => restored_session_metadata_from_record(&repaired),
            Err(error) => {
                tracing::warn!(
                    %error,
                    session_id = %session.session_id.0,
                    "failed to repair existing session metadata while restoring supervisor"
                );
                // 修复 state 失败不影响本次内存恢复；至少保留已读到的名称和 root。
                restored_session_metadata_from_record(record)
            }
        }
    }

    fn default_restored_session_root(&self) -> PathBuf {
        if let Some(root) = self
            .config
            .default_working_directory
            .as_ref()
            .and_then(|path| path.canonicalize().ok())
        {
            return root;
        }

        if let Ok(root) = std::env::current_dir().and_then(|path| path.canonicalize()) {
            return root;
        }

        // 极端环境下当前目录不可读时，退回系统临时目录，确保文件树根仍指向真实目录。
        std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
    }

    fn mark_persisted_session_closed(&mut self, session_id: SessionId) {
        self.close_visible_session_state(session_id);
        let now_ms = current_unix_timestamp_millis();
        if let Err(error) = self
            .client_history
            .record_session_closed(session_id, now_ms)
        {
            tracing::warn!(
                %error,
                session_id = %session_id.0,
                "failed to mark restored session closed in sqlite history"
            );
        }
        if let Err(error) = self.client_history.remove_session_attachments(session_id) {
            tracing::warn!(
                %error,
                session_id = %session_id.0,
                "failed to clear restored session attachments from sqlite history"
            );
        }
        if let Err(error) =
            StateStore::record_runtime_session_closed(&self.config.state_path, session_id, now_ms)
        {
            tracing::warn!(
                %error,
                session_id = %session_id.0,
                "failed to mark restored runtime session closed in sqlite state"
            );
        }
    }
}

fn restored_session_metadata_from_record(record: &SessionHistoryRecord) -> RestoredSessionMetadata {
    RestoredSessionMetadata {
        name: record.name.clone(),
        root_path: PathBuf::from(&record.root_path),
    }
}

fn default_restored_session_name(session_id: SessionId) -> String {
    let raw = session_id.0.to_string();
    format!("restored-{}", &raw[..8])
}

fn restore_info_is_reconnectable(restore_info: Option<&PtyRestoreInfo>) -> bool {
    matches!(
        restore_info,
        Some(PtyRestoreInfo::Tmux { .. })
            | Some(PtyRestoreInfo::UnixSocket {
                supervisor_status: PtySupervisorStatus::Running,
                ..
            })
    )
}
