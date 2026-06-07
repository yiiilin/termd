#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
use termd_proto::{SessionId, SessionState, TerminalSize, UnixTimestampMillis};
use tracing::warn;

use crate::pty::supervisor::SupervisorPtyBackend;
#[cfg(test)]
use crate::pty::supervisor::SupervisorRestoreCandidate;
#[cfg(test)]
use crate::pty::{PtyRestoreInfo, PtySupervisorStatus};
#[cfg(test)]
use crate::state::{DaemonState, SessionStateRecord};

pub(super) fn warn_about_orphaned_supervisors<I, S>(
    backend: &SupervisorPtyBackend,
    valid_session_ids: I,
) where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match backend.orphaned_supervisor_count(valid_session_ids) {
        Ok(orphaned_count) if orphaned_count > 0 => {
            // 启动/升级恢复路径绝不能因为判断为孤儿就主动 SIGTERM supervisor。
            // 如果 socket 文件临时缺失或状态迁移失败，里面仍可能是用户正在跑的 shell。
            warn!(
                orphaned_count,
                "left orphaned session supervisors running during startup"
            );
        }
        Ok(_) => {}
        Err(error) => warn!(%error, "failed to inspect orphaned session supervisors"),
    }
}

#[cfg(test)]
pub(crate) fn adopt_or_repair_runtime_sessions_from_supervisors(
    state: &mut DaemonState,
    supervisors: impl IntoIterator<Item = SupervisorRestoreCandidate>,
    now_ms: UnixTimestampMillis,
) -> usize {
    let mut session_positions = state
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.session_id, index))
        .collect::<HashMap<_, _>>();
    let mut repaired_count = 0;

    for supervisor in supervisors {
        let Ok(raw_session_id) = uuid::Uuid::parse_str(&supervisor.session_id) else {
            continue;
        };
        let session_id = SessionId(raw_session_id);
        let mut restored_session = SessionStateRecord {
            session_id,
            state: SessionState::Running,
            size: TerminalSize {
                rows: supervisor.size.rows,
                cols: supervisor.size.cols,
                pixel_width: supervisor.size.pixel_width,
                pixel_height: supervisor.size.pixel_height,
            },
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            restore_info: Some(PtyRestoreInfo::UnixSocket {
                socket_path: supervisor.socket_path,
                supervisor_pid: supervisor.supervisor_pid,
                supervisor_status: PtySupervisorStatus::Running,
            }),
        };

        if let Some(index) = session_positions.get(&session_id).copied() {
            let existing_session = &mut state.sessions[index];
            // live supervisor 是 runtime 事实来源。旧安装脚本或异常重启可能已经把 SQLite
            // runtime 行误标成 closed / 去掉 restore_info；supervisor 仍在时必须修回 Running，
            // 否则 daemon 重启会把用户还在运行的 shell 从 session 列表里“丢掉”。
            let needs_repair = existing_session.state != SessionState::Running
                || !restore_info_is_running_supervisor(existing_session.restore_info.as_ref());
            if needs_repair {
                restored_session.created_at_ms = existing_session.created_at_ms;
                *existing_session = restored_session;
                repaired_count += 1;
            }
            continue;
        }

        state.sessions.push(restored_session);
        session_positions.insert(session_id, state.sessions.len() - 1);
        repaired_count += 1;
    }

    repaired_count
}

#[cfg(test)]
fn restore_info_is_running_supervisor(restore_info: Option<&PtyRestoreInfo>) -> bool {
    matches!(
        restore_info,
        Some(PtyRestoreInfo::UnixSocket {
            supervisor_status: PtySupervisorStatus::Running,
            ..
        })
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use termd_proto::{SessionId, SessionState, TerminalSize, UnixTimestampMillis};

    use super::adopt_or_repair_runtime_sessions_from_supervisors;
    use crate::pty::supervisor::SupervisorRestoreCandidate;
    use crate::pty::{PtyRestoreInfo, PtySize, PtySupervisorStatus};
    use crate::state::{DaemonState, SessionStateRecord};

    #[test]
    fn missing_runtime_rows_are_adopted_from_live_supervisors_before_cleanup() {
        let session_id = SessionId::new();
        let socket_path = PathBuf::from(format!(
            "/var/lib/termd/termd-supervisors/{}.sock",
            session_id.0
        ));
        let mut state = DaemonState::default();
        let candidates = vec![SupervisorRestoreCandidate {
            session_id: session_id.0.to_string(),
            socket_path: socket_path.clone(),
            supervisor_pid: 4242,
            size: PtySize::with_pixels(35, 120, 1600, 1000),
        }];

        let adopted = adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            candidates,
            UnixTimestampMillis(12_345),
        );

        assert_eq!(adopted, 1);
        assert_eq!(state.sessions.len(), 1);
        let adopted_session = &state.sessions[0];
        assert_eq!(adopted_session.session_id, session_id);
        assert_eq!(adopted_session.state, SessionState::Running);
        assert_eq!(adopted_session.size.rows, 35);
        assert_eq!(adopted_session.size.cols, 120);
        assert_eq!(adopted_session.size.pixel_width, 1600);
        assert_eq!(adopted_session.size.pixel_height, 1000);
        assert_eq!(adopted_session.created_at_ms, UnixTimestampMillis(12_345));
        assert!(adopted_session.restore_info.is_some());
    }

    #[test]
    fn closed_runtime_rows_are_repaired_from_live_supervisors_before_cleanup() {
        let session_id = SessionId::new();
        let socket_path = PathBuf::from(format!(
            "/var/lib/termd/termd-supervisors/{}.sock",
            session_id.0
        ));
        let mut state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Closed,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_500),
                restore_info: None,
            }],
        };
        let candidates = vec![SupervisorRestoreCandidate {
            session_id: session_id.0.to_string(),
            socket_path: socket_path.clone(),
            supervisor_pid: 4242,
            size: PtySize::with_pixels(35, 120, 1600, 1000),
        }];

        let adopted = adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            candidates,
            UnixTimestampMillis(12_345),
        );

        assert_eq!(adopted, 1);
        assert_eq!(state.sessions.len(), 1);
        let repaired_session = &state.sessions[0];
        assert_eq!(repaired_session.session_id, session_id);
        assert_eq!(repaired_session.state, SessionState::Running);
        assert_eq!(repaired_session.size.rows, 35);
        assert_eq!(repaired_session.size.cols, 120);
        assert_eq!(repaired_session.created_at_ms, UnixTimestampMillis(1_000));
        assert_eq!(repaired_session.updated_at_ms, UnixTimestampMillis(12_345));
        assert!(repaired_session.restore_info.is_some());
    }

    #[test]
    fn running_rows_with_live_supervisor_are_left_untouched() {
        let session_id = SessionId::new();
        let socket_path = PathBuf::from(format!(
            "/var/lib/termd/termd-supervisors/{}.sock",
            session_id.0
        ));
        let existing_restore_info = PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid: 123,
            supervisor_status: PtySupervisorStatus::Running,
        };
        let mut state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_500),
                restore_info: Some(existing_restore_info.clone()),
            }],
        };

        let adopted = adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            vec![SupervisorRestoreCandidate {
                session_id: session_id.0.to_string(),
                socket_path,
                supervisor_pid: 4242,
                size: PtySize::with_pixels(35, 120, 1600, 1000),
            }],
            UnixTimestampMillis(12_345),
        );

        assert_eq!(adopted, 0);
        assert_eq!(state.sessions[0].created_at_ms, UnixTimestampMillis(1_000));
        assert_eq!(state.sessions[0].updated_at_ms, UnixTimestampMillis(1_500));
        assert_eq!(state.sessions[0].restore_info, Some(existing_restore_info));
    }
}
