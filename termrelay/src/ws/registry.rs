use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termd_proto::{Nonce, RelayClientId, RelayControlEnvelope, ServerId};
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tracing::{debug, trace, warn};

use super::{
    ConnectionId, ConnectionRole, FrameSender, OpaqueFrame, PENDING_CLIENTS_PER_ROOM_LIMIT,
    PRE_PAIR_CLIENT_BUFFER_MAX_BYTES, PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES,
    PRE_PAIR_ROOM_BUFFER_MAX_BYTES, RelayDataSendError, RelayOutbound, RoutePrelude,
    relay_control_frame,
};

#[derive(Debug, Default)]
pub(super) struct RelayRegistry {
    rooms: Mutex<HashMap<ServerId, RelayRoom>>,
    next_connection_id: AtomicU64,
}

#[derive(Debug, Default)]
struct RelayRoom {
    daemon_control: Option<ConnectionEndpoint>,
    route_generation: Option<Nonce>,
    daemon_data: HashMap<ConnectionId, ConnectionEndpoint>,
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
    pre_pair_client_bytes: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ClientCloseOutcome {
    removed: bool,
    notified_data_pipe: bool,
    pair_summary: Option<ClientPairLogContext>,
}

#[derive(Debug, Default, Clone)]
struct PrePairBuffer {
    frames: VecDeque<OpaqueFrame>,
    bytes: usize,
}

impl PrePairBuffer {
    fn push(&mut self, frame: OpaqueFrame) -> Result<(), OpaqueFrame> {
        let frame_len = frame.len();
        if self.frames.len() >= PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES
            || self.bytes.saturating_add(frame_len) > PRE_PAIR_CLIENT_BUFFER_MAX_BYTES
        {
            return Err(frame);
        }
        self.bytes = self.bytes.saturating_add(frame_len);
        self.frames.push_back(frame);
        Ok(())
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn drain(&mut self) -> Vec<OpaqueFrame> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

impl RelayRoom {
    fn is_empty(&self) -> bool {
        self.daemon_control.is_none() && self.daemon_data.is_empty() && self.clients.is_empty()
    }

    fn close_clients(&mut self) {
        for (_, client) in self.clients.drain() {
            // daemon control 不可用时，client 必须尽快收到 close，避免继续等待业务响应直到超时。
            if let Some(data_id) = client.paired_daemon_data_id
                && let Some(daemon_data) = self.daemon_data.remove(&data_id)
            {
                daemon_data.sender.request_close();
            }
            client.pair_signal.notify_waiters();
            client.sender.request_close();
        }
        self.pre_pair_client_bytes = 0;
        for (_, daemon_data) in self.daemon_data.drain() {
            daemon_data.sender.request_close();
        }
    }

    fn clear_daemon_control_and_dependents(&mut self) {
        if let Some(daemon_control) = self.daemon_control.take() {
            // 中文注释：control 线是 daemon 是否在线的唯一裁定来源；它断开时，
            // 所有关联 browser/data transport 都必须关闭，让 browser 重新建链。
            daemon_control.sender.request_close();
        }
        self.route_generation = None;
        self.close_clients();
    }

    fn close_client_transport(&mut self, client_id: ConnectionId) -> ClientCloseOutcome {
        let Some(client) = self.clients.remove(&client_id) else {
            return ClientCloseOutcome::default();
        };

        let mut outcome = ClientCloseOutcome {
            removed: true,
            notified_data_pipe: false,
            pair_summary: Some(client_pair_log_context(&client, Instant::now())),
        };
        self.release_client_pre_pair_bytes(&client);
        client.pair_signal.notify_waiters();
        if let Some(data_id) = client.paired_daemon_data_id {
            if let Some(daemon_data) = self.daemon_data.remove(&data_id) {
                // 中文注释：daemon data pipe 与 client 一对一绑定。client 断开时直接关闭
                // 对应 data WebSocket，后续 client 必须重新走 control 线 OpenData 回连，
                // 避免旧 client 的残留帧污染下一次 attach/upload。
                outcome.notified_data_pipe = true;
                daemon_data.sender.request_close();
            }
        }

        client.sender.request_close();
        outcome
    }

    pub(super) fn buffer_client_frame(
        &mut self,
        client_id: ConnectionId,
        frame: OpaqueFrame,
    ) -> Result<(), OpaqueFrame> {
        if !self.clients.contains_key(&client_id) {
            return Err(frame);
        }

        let frame_len = frame.len();
        if self.pre_pair_client_bytes.saturating_add(frame_len) > PRE_PAIR_ROOM_BUFFER_MAX_BYTES {
            return Err(frame);
        }

        let client = self
            .clients
            .get_mut(&client_id)
            .expect("client existence was checked before pre-pair buffering");
        client.pre_pair_buffer.push(frame)?;
        self.pre_pair_client_bytes = self.pre_pair_client_bytes.saturating_add(frame_len);
        Ok(())
    }

    pub(super) fn drain_pre_pair_client_frames(
        &mut self,
        client_id: ConnectionId,
    ) -> Option<Vec<OpaqueFrame>> {
        let client = self.clients.get_mut(&client_id)?;
        let bytes = client.pre_pair_buffer.bytes();
        let frames = client.pre_pair_buffer.drain();
        self.pre_pair_client_bytes = self.pre_pair_client_bytes.saturating_sub(bytes);
        Some(frames)
    }

    fn notify_client_disconnected_to_control(&mut self, client_id: ConnectionId) {
        let Some(daemon_control) = self.daemon_control.as_ref() else {
            return;
        };
        let envelope = RelayControlEnvelope::ClientDisconnected {
            client_id: RelayClientId(client_id),
        };
        if daemon_control
            .sender
            .try_send_control(RelayOutbound::Frame(relay_control_frame(envelope)))
            .is_err()
        {
            // control 线连生命周期消息都写不进时，保留 room 只会留下假在线状态。
            self.clear_daemon_control_and_dependents();
        }
    }

    fn pending_client_count(&self) -> usize {
        self.clients
            .values()
            .filter(|client| client.paired_daemon_data_id.is_none())
            .count()
    }

    fn release_client_pre_pair_bytes(&mut self, client: &ConnectionEndpoint) {
        self.pre_pair_client_bytes = self
            .pre_pair_client_bytes
            .saturating_sub(client.pre_pair_buffer.bytes());
    }
}

#[derive(Debug, Clone)]
struct ConnectionEndpoint {
    id: ConnectionId,
    sender: FrameSender,
    data_token: Option<Nonce>,
    paired_daemon_data_id: Option<ConnectionId>,
    paired_client_id: Option<ConnectionId>,
    pre_pair_buffer: PrePairBuffer,
    pre_pair_flush_in_progress: bool,
    pair_signal: Arc<Notify>,
    created_at: Instant,
    paired_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientPairLogContext {
    pub(super) wait_ms: u64,
    pub(super) paired: bool,
    pub(super) pre_pair_frames: usize,
    pub(super) pre_pair_bytes: usize,
}

fn duration_ms_saturated(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn client_pair_log_context(client: &ConnectionEndpoint, now: Instant) -> ClientPairLogContext {
    ClientPairLogContext {
        wait_ms: duration_ms_saturated(now.saturating_duration_since(client.created_at)),
        paired: client.paired_daemon_data_id.is_some(),
        pre_pair_frames: client.pre_pair_buffer.frames.len(),
        pre_pair_bytes: client.pre_pair_buffer.bytes(),
    }
}

impl ConnectionEndpoint {
    fn new(id: ConnectionId, sender: FrameSender) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: None,
            pre_pair_buffer: PrePairBuffer::default(),
            pre_pair_flush_in_progress: false,
            pair_signal: Arc::new(Notify::new()),
            created_at: Instant::now(),
            paired_at: None,
        }
    }

    fn new_client(id: ConnectionId, sender: FrameSender, data_token: Nonce) -> Self {
        Self {
            id,
            sender,
            data_token: Some(data_token),
            paired_daemon_data_id: None,
            paired_client_id: None,
            pre_pair_buffer: PrePairBuffer::default(),
            pre_pair_flush_in_progress: false,
            pair_signal: Arc::new(Notify::new()),
            created_at: Instant::now(),
            paired_at: None,
        }
    }

    fn new_daemon_data(id: ConnectionId, sender: FrameSender, client_id: ConnectionId) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: Some(client_id),
            pre_pair_buffer: PrePairBuffer::default(),
            pre_pair_flush_in_progress: false,
            pair_signal: Arc::new(Notify::new()),
            created_at: Instant::now(),
            paired_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConnectionRegistration {
    pub(super) server_id: ServerId,
    pub(super) role: ConnectionRole,
    pub(super) id: ConnectionId,
    pub(super) route_generation: Option<Nonce>,
    pub(super) paired_client_id: Option<ConnectionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ForwardReport {
    pub(super) attempted: usize,
    pub(super) delivered: usize,
    pub(super) dropped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RelayForwardOutcome {
    pub(super) report: ForwardReport,
    pub(super) should_continue: bool,
}

impl RelayForwardOutcome {
    pub(super) fn continue_with(report: ForwardReport) -> Self {
        Self {
            report,
            should_continue: true,
        }
    }

    pub(super) fn close_with(report: ForwardReport) -> Self {
        Self {
            report,
            should_continue: false,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum RelayError {
    #[error("daemon route is missing route_generation")]
    DaemonRouteGenerationRequired,
    #[error("daemon control is not connected for server_id")]
    DaemonControlOffline,
    #[error("daemon control channel is backpressured")]
    DaemonControlBusy,
    #[error("relay room has too many pending clients")]
    PendingClientLimitExceeded,
    #[error("daemon data route is missing client_id or data_token")]
    DaemonDataRouteInvalid,
    #[error("daemon data route does not match a pending client")]
    DaemonDataRouteRejected,
    #[error("relay admission is required")]
    AdmissionRequired,
    #[error("relay admission was rejected")]
    AdmissionRejected,
    #[error("relay state mutex poisoned")]
    Poisoned,
}

impl RelayError {
    pub(super) fn route_error_code(&self) -> &'static str {
        match self {
            Self::DaemonRouteGenerationRequired => "relay_route_generation_required",
            Self::DaemonControlOffline => "relay_daemon_offline",
            Self::DaemonControlBusy => "relay_busy",
            Self::PendingClientLimitExceeded => "relay_pending_client_limit",
            Self::DaemonDataRouteInvalid => "relay_data_route_invalid",
            Self::DaemonDataRouteRejected => "relay_data_route_rejected",
            Self::AdmissionRequired => "relay_admission_required",
            Self::AdmissionRejected => "relay_admission_rejected",
            Self::Poisoned => "relay_state_unavailable",
        }
    }

    pub(super) fn route_error_message(&self) -> &'static str {
        match self {
            Self::DaemonRouteGenerationRequired => {
                "relay daemon routes must include route_generation"
            }
            Self::DaemonControlOffline => {
                "relay daemon control is not connected; retry after daemon reconnects"
            }
            Self::DaemonControlBusy => "relay daemon control is busy; retry shortly",
            Self::PendingClientLimitExceeded => {
                "relay room has too many clients waiting for daemon data; retry shortly"
            }
            Self::DaemonDataRouteInvalid => "relay daemon data route is invalid",
            Self::DaemonDataRouteRejected => "relay daemon data route was rejected",
            Self::AdmissionRequired => "relay admission is required",
            Self::AdmissionRejected => "relay admission was rejected",
            Self::Poisoned => "relay state is temporarily unavailable",
        }
    }
}

impl RelayRegistry {
    fn remove_room_if_empty(rooms: &mut HashMap<ServerId, RelayRoom>, server_id: ServerId) {
        if rooms.get(&server_id).is_some_and(RelayRoom::is_empty) {
            rooms.remove(&server_id);
        }
    }

    pub(super) fn room_count(&self) -> usize {
        self.rooms
            .lock()
            .expect("relay registry mutex poisoned")
            .len()
    }

    pub(super) fn register(
        &self,
        prelude: &RoutePrelude,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let server_id = prelude.server_id;
        let role = prelude.connection_role;
        let id = self.next_connection_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut rooms = self.rooms.lock().map_err(|_| RelayError::Poisoned)?;

        if matches!(
            role,
            ConnectionRole::DaemonControl | ConnectionRole::DaemonData
        ) && prelude.route_generation.is_none()
        {
            // 中文注释：daemon route 必须显式声明代际；否则 relay 无法在边界层拒绝
            // 旧代迟到接入的 data pipe，只能退化为依赖发送端“自觉带字段”。
            return Err(RelayError::DaemonRouteGenerationRequired);
        }

        match role {
            ConnectionRole::DaemonControl => {
                let room = rooms.entry(server_id).or_default();
                let endpoint = ConnectionEndpoint::new(id, sender);
                let route_generation = prelude.route_generation.clone();
                if let Some(stale_control) = room.daemon_control.replace(endpoint) {
                    warn!(
                        server_id = %server_id.0,
                        stale_connection_id = stale_control.id,
                        new_connection_id = id,
                        "replacing stale relay daemon control"
                    );
                    stale_control.sender.request_close();
                    room.close_clients();
                }
                room.route_generation = route_generation;
                debug!(
                    server_id = %server_id.0,
                    connection_id = id,
                    client_count = room.clients.len(),
                    "relay registered daemon control"
                );
            }
            ConnectionRole::DaemonData => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonControlOffline)?;
                if room
                    .route_generation
                    .as_ref()
                    .is_some_and(|current| prelude.route_generation.as_ref() != Some(current))
                {
                    return Err(RelayError::DaemonDataRouteRejected);
                }
                match (prelude.client_id, prelude.data_token.as_ref()) {
                    (Some(client_id), Some(data_token)) => {
                        let client_id = client_id.0;
                        let Some(client) = room.clients.get_mut(&client_id) else {
                            return Err(RelayError::DaemonDataRouteRejected);
                        };
                        if client.data_token.as_ref() != Some(data_token)
                            || client.paired_daemon_data_id.is_some()
                        {
                            return Err(RelayError::DaemonDataRouteRejected);
                        }
                        client.paired_daemon_data_id = Some(id);
                        let paired_at = Instant::now();
                        client.paired_at = Some(paired_at);
                        let pair_wait_ms = duration_ms_saturated(
                            paired_at.saturating_duration_since(client.created_at),
                        );
                        let pre_pair_frames = client.pre_pair_buffer.frames.len();
                        let pre_pair_bytes = client.pre_pair_buffer.bytes();
                        // 中文注释：data pipe 已经完成身份配对，但配对前 client 可能已有
                        // 业务帧排在预缓冲里。flush 完成前，新来的帧仍必须继续进入同一
                        // 预缓冲队列，否则会越过旧帧直达 daemon data。
                        client.pre_pair_flush_in_progress = !client.pre_pair_buffer.is_empty();
                        client.pair_signal.notify_waiters();
                        room.daemon_data.insert(
                            id,
                            ConnectionEndpoint::new_daemon_data(id, sender, client_id),
                        );
                        debug!(
                            server_id = %server_id.0,
                            connection_id = id,
                            client_connection_id = client_id,
                            pair_wait_ms,
                            pre_pair_frames,
                            pre_pair_bytes,
                            "relay registered daemon data pipe"
                        );
                    }
                    _ => return Err(RelayError::DaemonDataRouteInvalid),
                }
            }
            ConnectionRole::Client => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonControlOffline)?;
                if room.pending_client_count() >= PENDING_CLIENTS_PER_ROOM_LIMIT {
                    return Err(RelayError::PendingClientLimitExceeded);
                }
                let data_token = Nonce(format!("relay-data-{}-{id}", uuid::Uuid::new_v4()));
                let control_sender = room
                    .daemon_control
                    .as_ref()
                    .ok_or(RelayError::DaemonControlOffline)?
                    .sender
                    .clone();
                let open_data = RelayControlEnvelope::OpenData {
                    client_id: RelayClientId(id),
                    data_token: data_token.clone(),
                };
                // 中文注释：必须先把 pending client 放进 room，再通知 daemon 反连 data。
                // 否则 control writer 足够快时，daemon data 可能早于 client 入表到达并被误拒。
                room.clients
                    .insert(id, ConnectionEndpoint::new_client(id, sender, data_token));
                if room.pending_client_count() > PENDING_CLIENTS_PER_ROOM_LIMIT {
                    room.close_client_transport(id);
                    return Err(RelayError::PendingClientLimitExceeded);
                }
                match control_sender
                    .try_send_control(RelayOutbound::Frame(relay_control_frame(open_data)))
                {
                    Ok(()) => {
                        debug!(
                            server_id = %server_id.0,
                            connection_id = id,
                            client_count = room.clients.len(),
                            "relay registered pending client data pipe"
                        );
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        room.close_client_transport(id);
                        return Err(RelayError::DaemonControlBusy);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        room.close_client_transport(id);
                        room.clear_daemon_control_and_dependents();
                        return Err(RelayError::DaemonControlOffline);
                    }
                }
            }
        }

        Ok(ConnectionRegistration {
            server_id,
            role,
            id,
            route_generation: rooms
                .get(&server_id)
                .and_then(|room| room.route_generation.clone())
                .or_else(|| prelude.route_generation.clone()),
            paired_client_id: if role == ConnectionRole::DaemonData {
                prelude.client_id.map(|client_id| client_id.0)
            } else {
                None
            },
        })
    }

    pub(super) fn unregister(&self, registration: &ConnectionRegistration) {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during unregister");
            return;
        };

        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return;
        };

        match registration.role {
            ConnectionRole::DaemonControl => {
                if room
                    .daemon_control
                    .as_ref()
                    .is_some_and(|daemon| daemon.id == registration.id)
                {
                    debug!(
                        server_id = %registration.server_id.0,
                        connection_id = registration.id,
                        client_count = room.clients.len(),
                        "relay unregistering daemon control"
                    );
                    room.clear_daemon_control_and_dependents();
                }
            }
            ConnectionRole::DaemonData => {
                let removed_data = room.daemon_data.remove(&registration.id);
                let removed = removed_data.is_some();
                let paired_client_id = removed_data
                    .as_ref()
                    .and_then(|data| data.paired_client_id)
                    .or(registration.paired_client_id);
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    paired_client_id,
                    "relay unregistering daemon data"
                );
                if let Some(data) = removed_data {
                    data.sender.request_close();
                }
                if removed
                    && let Some(client_id) = paired_client_id
                    && let Some(client) = room.clients.get(&client_id)
                    && client.paired_daemon_data_id == Some(registration.id)
                {
                    room.close_client_transport(client_id);
                }
            }
            ConnectionRole::Client => {
                let close = room.close_client_transport(registration.id);
                if let Some(summary) = close.pair_summary {
                    debug!(
                        server_id = %registration.server_id.0,
                        connection_id = registration.id,
                        remaining_clients = room.clients.len(),
                        pair_wait_ms = summary.wait_ms,
                        paired = summary.paired,
                        pre_pair_frames = summary.pre_pair_frames,
                        pre_pair_bytes = summary.pre_pair_bytes,
                        "relay unregistering client"
                    );
                } else {
                    debug!(
                        server_id = %registration.server_id.0,
                        connection_id = registration.id,
                        remaining_clients = room.clients.len(),
                        "relay unregistering client"
                    );
                }
                // 中文注释：已配对 data pipe 直接收到 client_disconnected；尚未配对的
                // pending client 只能通过 control 线通知 daemon 取消冷启动 data 任务。
                if close.removed && !close.notified_data_pipe {
                    room.notify_client_disconnected_to_control(registration.id);
                }
            }
        }

        Self::remove_room_if_empty(&mut rooms, registration.server_id);
    }

    pub(super) fn has_client(&self, server_id: ServerId, client_id: RelayClientId) -> bool {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client presence check");
            return false;
        };
        rooms
            .get(&server_id)
            .is_some_and(|room| room.clients.contains_key(&client_id.0))
    }

    #[cfg(test)]
    pub(super) fn client_has_data_pair(&self, registration: &ConnectionRegistration) -> bool {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client pair status lookup");
            return false;
        };
        rooms
            .get(&registration.server_id)
            .and_then(|room| room.clients.get(&registration.id))
            .is_some_and(|client| client.paired_daemon_data_id.is_some())
    }

    pub(super) fn close_pending_client_if_unpaired(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<ClientPairLogContext> {
        if registration.role != ConnectionRole::Client {
            return None;
        }

        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pending client deadline cleanup");
            return None;
        };
        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return None;
        };
        let Some(client) = room.clients.get(&registration.id) else {
            return None;
        };
        if client.paired_daemon_data_id.is_some() {
            return None;
        }

        let close = room.close_client_transport(registration.id);
        if close.removed && !close.notified_data_pipe {
            room.notify_client_disconnected_to_control(registration.id);
        }
        Self::remove_room_if_empty(&mut rooms, registration.server_id);
        if close.removed {
            close.pair_summary
        } else {
            None
        }
    }

    pub(super) async fn wait_client_data_pair(
        &self,
        registration: &ConnectionRegistration,
    ) -> bool {
        if registration.role != ConnectionRole::Client {
            return false;
        }
        loop {
            let pair_signal = {
                let Ok(rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during client pair wait");
                    return false;
                };
                let Some(room) = rooms.get(&registration.server_id) else {
                    return false;
                };
                let Some(client) = room.clients.get(&registration.id) else {
                    return false;
                };
                if client.paired_daemon_data_id.is_some() {
                    return true;
                }
                client.pair_signal.clone()
            };
            tokio::select! {
                _ = pair_signal.notified() => {}
                // 中文注释：Notify 不保存 notify_waiters 的历史事件；配对如果刚好发生在
                // clone signal 和 await 之间，短轮询能保证不会永远睡在冷启动上传路径上。
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            }
        }
    }

    pub(super) fn daemon_data_pair_wait_ms(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<u64> {
        if registration.role != ConnectionRole::DaemonData {
            return None;
        }
        let client_id = registration.paired_client_id?;
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon data pair wait lookup");
            return None;
        };
        let room = rooms.get(&registration.server_id)?;
        let client = room.clients.get(&client_id)?;
        let paired_at = client.paired_at?;
        Some(duration_ms_saturated(
            paired_at.saturating_duration_since(client.created_at),
        ))
    }

    pub(super) async fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        match registration.role {
            ConnectionRole::DaemonControl => self.drop_control_payload(registration).await,
            ConnectionRole::DaemonData => {
                self.forward_daemon_data_to_client(registration, frame)
                    .await
            }
            ConnectionRole::Client => {
                self.forward_client_to_daemon_data(registration, frame)
                    .await
            }
        }
    }

    async fn drop_control_payload(&self, registration: &ConnectionRegistration) -> ForwardReport {
        debug!(
            server_id = %registration.server_id.0,
            connection_id = registration.id,
            "dropping payload received on relay daemon control line"
        );
        ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        }
    }

    async fn forward_client_to_daemon_data(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (daemon_data_id, daemon_data_sender) = {
            let Ok(rooms) = self.rooms.lock() else {
                warn!("relay registry mutex poisoned during client data forward");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(room) = rooms.get(&registration.server_id) else {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(client) = room.clients.get(&registration.id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            if client.pre_pair_flush_in_progress {
                drop(rooms);
                return self.buffer_unpaired_client_frame(registration, frame);
            }
            let Some(data_id) = client.paired_daemon_data_id else {
                drop(rooms);
                return self.buffer_unpaired_client_frame(registration, frame);
            };
            let Some(daemon_data) = room.daemon_data.get(&data_id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            (daemon_data.id, daemon_data.sender.clone())
        };

        // 中文注释：这里不能 await daemon data 队列腾挪；当前 client 的读循环会被卡住，
        // Close 帧也就无法被读取和 unregister。目标队列承压时直接关闭当前 client，
        // 让上游重连，而不是把旧 client 悬挂在 relay 内部。
        match daemon_data_sender.try_send_data(RelayOutbound::Frame(frame)) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during daemon data cleanup");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                if let Some(room) = rooms.get_mut(&registration.server_id)
                    && room
                        .daemon_data
                        .get(&daemon_data_id)
                        .is_some_and(|endpoint| endpoint.id == daemon_data_id)
                {
                    room.close_client_transport(registration.id);
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    pub(super) async fn forward_client_to_daemon_data_backpressured(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (daemon_data_id, daemon_data_sender) =
            match self.client_daemon_data_sender(registration) {
                Some(pair) => pair,
                None => {
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                }
            };

        // 中文注释：HTTP tunnel 的 request body 在独立任务里读取，不会挡住 browser
        // WebSocket 的 Close/cleanup 读循环；文件体应直接承接 daemon data 线的真实背压。
        match daemon_data_sender
            .send_data(RelayOutbound::Frame(frame))
            .await
        {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                self.close_client_after_daemon_data_send_failure(registration, daemon_data_id)
            }
        }
    }

    fn client_daemon_data_sender(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<(u64, FrameSender)> {
        if registration.role != ConnectionRole::Client {
            return None;
        }
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client data sender lookup");
            return None;
        };
        let room = rooms.get(&registration.server_id)?;
        let client = room.clients.get(&registration.id)?;
        let data_id = client.paired_daemon_data_id?;
        let daemon_data = room.daemon_data.get(&data_id)?;
        Some((daemon_data.id, daemon_data.sender.clone()))
    }

    fn close_client_after_daemon_data_send_failure(
        &self,
        registration: &ConnectionRegistration,
        daemon_data_id: u64,
    ) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon data cleanup");
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        if let Some(room) = rooms.get_mut(&registration.server_id)
            && room
                .daemon_data
                .get(&daemon_data_id)
                .is_some_and(|endpoint| endpoint.id == daemon_data_id)
        {
            room.close_client_transport(registration.id);
        }
        Self::remove_room_if_empty(&mut rooms, registration.server_id);
        ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        }
    }

    fn buffer_unpaired_client_frame(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair client buffering");
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        match room.buffer_client_frame(registration.id, frame) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(_frame) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    max_frames = PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES,
                    max_bytes = PRE_PAIR_CLIENT_BUFFER_MAX_BYTES,
                    "relay pre-pair client buffer full"
                );
                let close = room.close_client_transport(registration.id);
                if close.removed && !close.notified_data_pipe {
                    room.notify_client_disconnected_to_control(registration.id);
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    pub(super) async fn flush_pre_pair_client_frames(&self, registration: &ConnectionRegistration) {
        if registration.role != ConnectionRole::DaemonData {
            return;
        }
        let Some(client_id) = registration.paired_client_id else {
            return;
        };
        let mut frame_count = 0_usize;
        let mut delivered = 0_usize;
        loop {
            let Some((frames, daemon_sender)) =
                self.take_pre_pair_client_frames(registration, client_id)
            else {
                return;
            };
            if frames.is_empty() {
                break;
            }
            frame_count = frame_count.saturating_add(frames.len());
            for frame in frames {
                match daemon_sender.send_data(RelayOutbound::Frame(frame)).await {
                    Ok(()) => delivered = delivered.saturating_add(1),
                    Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                        self.close_client_after_pre_pair_flush_failure(registration, client_id);
                        break;
                    }
                }
            }
        }
        debug!(
            server_id = %registration.server_id.0,
            daemon_data_connection_id = registration.id,
            client_connection_id = client_id,
            frames = frame_count,
            delivered,
            "relay flushed pre-pair client frames"
        );
    }

    fn take_pre_pair_client_frames(
        &self,
        registration: &ConnectionRegistration,
        client_id: ConnectionId,
    ) -> Option<(Vec<OpaqueFrame>, FrameSender)> {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair client flush");
            return None;
        };
        let room = rooms.get_mut(&registration.server_id)?;
        let daemon_sender = room.daemon_data.get(&registration.id)?.sender.clone();
        let client = room.clients.get(&client_id)?;
        if client.paired_daemon_data_id != Some(registration.id) {
            return None;
        }
        let frames = room.drain_pre_pair_client_frames(client_id)?;
        if frames.is_empty()
            && let Some(client) = room.clients.get_mut(&client_id)
            && client.paired_daemon_data_id == Some(registration.id)
        {
            // 中文注释：只在确认预缓冲已经为空时结束 flushing 阶段；此前新帧会继续
            // 进入预缓冲，确保配对前后的 client 帧按原始 FIFO 顺序送到 daemon。
            client.pre_pair_flush_in_progress = false;
        }
        Some((frames, daemon_sender))
    }

    fn close_client_after_pre_pair_flush_failure(
        &self,
        registration: &ConnectionRegistration,
        client_id: ConnectionId,
    ) {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair flush cleanup");
            return;
        };
        if let Some(room) = rooms.get_mut(&registration.server_id) {
            let close = room.close_client_transport(client_id);
            if close.removed && !close.notified_data_pipe {
                room.notify_client_disconnected_to_control(client_id);
            }
        }
        Self::remove_room_if_empty(&mut rooms, registration.server_id);
    }

    async fn forward_daemon_data_to_client(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (client_id, client_sender) = {
            let Ok(rooms) = self.rooms.lock() else {
                warn!("relay registry mutex poisoned during daemon data forward");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(room) = rooms.get(&registration.server_id) else {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(daemon_data) = room.daemon_data.get(&registration.id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            let Some(client_id) = daemon_data.paired_client_id else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            let Some(client) = room.clients.get(&client_id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            if client.paired_daemon_data_id != Some(registration.id) {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            }
            (client_id, client.sender.clone())
        };

        // 中文注释：daemon -> client 这条链路代表终端下行输出。这里如果像 client -> daemon
        // 一样在第一次 BudgetFull 就立刻断开，会把几百毫秒到几秒的公网抖动直接放大成
        // 浏览器重连和 full snapshot，用户体感就是 relay 比直连更容易“卡住后重刷”。
        //
        // 对下行链路更合理的策略是让背压沿着 relay 回传到 daemon data WebSocket：
        // 当前读循环在这里短暂 await，内核 socket buffer 会继续承接一段数据；如果浏览器
        // 真正长时间不可写，writer 侧的 send deadline/close signal 仍会最终打断这里。
        match client_sender.send_data(RelayOutbound::Frame(frame)).await {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::Closed) | Err(RelayDataSendError::BudgetFull) => {
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during slow client data cleanup");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                if let Some(room) = rooms.get_mut(&registration.server_id) {
                    let close = room.close_client_transport(client_id);
                    if close.removed && !close.notified_data_pipe {
                        room.notify_client_disconnected_to_control(client_id);
                    }
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    pub(super) fn queue_pong_for_registration(
        &self,
        registration: &ConnectionRegistration,
        payload: Vec<u8>,
    ) -> RelayForwardOutcome {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during ping pong enqueue");
            return RelayForwardOutcome::close_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        };
        let sender = rooms
            .get(&registration.server_id)
            .and_then(|room| match registration.role {
                ConnectionRole::DaemonControl => {
                    room.daemon_control.as_ref().and_then(|endpoint| {
                        (endpoint.id == registration.id).then_some(endpoint.sender.clone())
                    })
                }
                ConnectionRole::DaemonData => room
                    .daemon_data
                    .get(&registration.id)
                    .map(|endpoint| endpoint.sender.clone()),
                ConnectionRole::Client => room
                    .clients
                    .get(&registration.id)
                    .map(|endpoint| endpoint.sender.clone()),
            });
        drop(rooms);
        let Some(sender) = sender else {
            return RelayForwardOutcome::close_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        };
        match sender.try_send_control(RelayOutbound::Pong(payload)) {
            Ok(()) => {
                trace!(
                    server_id = %registration.server_id.0,
                    ?registration.role,
                    connection_id = registration.id,
                    "relay queued pong for inbound ping"
                );
                RelayForwardOutcome::continue_with(ForwardReport {
                    attempted: 1,
                    delivered: 1,
                    dropped: 0,
                })
            }
            Err(error) => {
                warn!(
                    server_id = %registration.server_id.0,
                    ?registration.role,
                    connection_id = registration.id,
                    %error,
                    "relay failed to queue pong for inbound ping"
                );
                RelayForwardOutcome::close_with(ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                })
            }
        }
    }
}
