use axum::extract::ws::{Message, WebSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use termd_proto::ServerId;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use super::policy::{
    OutboundFramePressureLevel, WEBSOCKET_IDLE_PING_INTERVAL, WEBSOCKET_SEND_DEADLINE,
    send_message_with_deadline, websocket_idle_ping_due, websocket_outbound_frame_pressure_level,
};
use super::{
    CONTROL_CHANNEL_CAPACITY, ConnectionId, ConnectionRole, DATA_CHANNEL_BYTE_BUDGET, OpaqueFrame,
};

#[derive(Debug, Clone)]
pub(super) struct FrameSender {
    control: mpsc::Sender<RelayOutbound>,
    data: mpsc::Sender<RelayOutbound>,
    pub(super) data_budget: Arc<DataQueueByteBudget>,
    close_signal: EndpointCloseSignal,
}

impl FrameSender {
    pub(super) fn channel(
        data_capacity: usize,
    ) -> (
        Self,
        mpsc::Receiver<RelayOutbound>,
        mpsc::Receiver<RelayOutbound>,
    ) {
        Self::channel_with_capacities(CONTROL_CHANNEL_CAPACITY, data_capacity)
    }

    pub(super) fn channel_with_capacities(
        control_capacity: usize,
        data_capacity: usize,
    ) -> (
        Self,
        mpsc::Receiver<RelayOutbound>,
        mpsc::Receiver<RelayOutbound>,
    ) {
        let (control_tx, control_rx) = mpsc::channel(control_capacity);
        let (data_tx, data_rx) = mpsc::channel(data_capacity);
        let data_budget = Arc::new(DataQueueByteBudget::new(DATA_CHANNEL_BYTE_BUDGET));
        let close_signal = EndpointCloseSignal::new();
        (
            Self {
                control: control_tx,
                data: data_tx,
                data_budget,
                close_signal,
            },
            control_rx,
            data_rx,
        )
    }

    pub(super) fn try_send_data(&self, outbound: RelayOutbound) -> Result<(), RelayDataSendError> {
        let queued_bytes = outbound.queued_data_bytes();
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        if !self.data_budget.try_reserve(queued_bytes) {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data queue byte budget exhausted"
            );
            return Err(RelayDataSendError::BudgetFull);
        }
        match self.data.try_send(outbound) {
            Ok(()) => {
                trace!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue accepted frame"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue full"
                );
                Err(RelayDataSendError::BudgetFull)
            }
            Err(mpsc::error::TrySendError::Closed(_outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue closed"
                );
                Err(RelayDataSendError::Closed)
            }
        }
    }

    pub(super) async fn send_data(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), RelayDataSendError> {
        let queued_bytes = outbound.queued_data_bytes();
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        if self.data_budget.exceeds_limit(queued_bytes) {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data frame exceeds byte budget"
            );
            return Err(RelayDataSendError::BudgetFull);
        }
        let mut close_rx = self.subscribe_close();
        let budget_reserved = tokio::select! {
            biased;

            _ = self.data.closed() => false,
            reserved = self.data_budget.reserve_or_wait(queued_bytes, &mut close_rx) => reserved,
        };
        if !budget_reserved {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data queue closed while waiting for byte budget"
            );
            return Err(RelayDataSendError::Closed);
        }

        let permit = tokio::select! {
            biased;

            _ = close_rx.closed() => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind,
                    queued_bytes,
                    "relay data queue closed while waiting for channel capacity"
                );
                return Err(RelayDataSendError::Closed);
            }
            permit = self.data.reserve() => permit,
        };

        match permit {
            Ok(permit) => {
                permit.send(outbound);
                trace!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue accepted frame"
                );
                Ok(())
            }
            Err(_closed) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue closed"
                );
                Err(RelayDataSendError::Closed)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn try_send(&self, outbound: RelayOutbound) -> Result<(), RelayDataSendError> {
        self.try_send_data(outbound)
    }

    pub(super) fn try_send_control(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), mpsc::error::TrySendError<RelayOutbound>> {
        // 生命周期控制消息不能被普通业务队列挤掉；但它也必须有上限。
        // 如果底层 WebSocket 慢写到 control 都堆满，继续无界缓存只会拖垮整个 relay。
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        match self.control.try_send(outbound) {
            Ok(()) => {
                trace!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue accepted frame"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(outbound)) => {
                warn!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue full"
                );
                Err(mpsc::error::TrySendError::Full(outbound))
            }
            Err(mpsc::error::TrySendError::Closed(outbound)) => {
                warn!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue closed"
                );
                Err(mpsc::error::TrySendError::Closed(outbound))
            }
        }
    }

    pub(super) fn subscribe_close(&self) -> EndpointCloseReceiver {
        self.close_signal.subscribe()
    }

    pub(super) fn close_endpoint(&self) {
        self.close_signal.close();
    }

    pub(super) fn request_close(&self) {
        // 中文注释：close 信号是可靠退出路径；队列里的 Close 只是尽力发送 WebSocket
        // close frame。即使 control 队列已满，endpoint 也会通过信号退出。
        self.close_endpoint();
        let _ = self.try_send_control(RelayOutbound::Close);
    }
}

#[derive(Debug)]
pub(super) struct PipePump {
    sender: FrameSender,
    control_rx: mpsc::Receiver<RelayOutbound>,
    data_rx: mpsc::Receiver<RelayOutbound>,
}

impl PipePump {
    pub(super) fn new(data_capacity: usize) -> Self {
        let (sender, control_rx, data_rx) = FrameSender::channel(data_capacity);
        Self {
            sender,
            control_rx,
            data_rx,
        }
    }

    #[cfg(test)]
    pub(super) fn with_capacities(control_capacity: usize, data_capacity: usize) -> Self {
        let (sender, control_rx, data_rx) =
            FrameSender::channel_with_capacities(control_capacity, data_capacity);
        Self {
            sender,
            control_rx,
            data_rx,
        }
    }

    pub(super) fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    pub(super) fn spawn_writer(
        self,
        sender: futures_util::stream::SplitSink<WebSocket, Message>,
        server_id: ServerId,
        role: ConnectionRole,
        connection_id: ConnectionId,
    ) -> tokio::task::JoinHandle<()> {
        let endpoint = self.sender.clone();
        tokio::spawn(run_relay_websocket_writer(
            sender,
            server_id,
            role,
            connection_id,
            self.control_rx,
            self.data_rx,
            self.sender.data_budget.clone(),
            self.sender.subscribe_close(),
            endpoint,
        ))
    }

    pub(super) fn into_data_receiver(self) -> PumpDataReceiver {
        PumpDataReceiver {
            receiver: self.data_rx,
            data_budget: self.sender.data_budget.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn into_test_parts(
        self,
    ) -> (FrameSender, mpsc::Receiver<RelayOutbound>, PumpDataReceiver) {
        (
            self.sender.clone(),
            self.control_rx,
            PumpDataReceiver {
                receiver: self.data_rx,
                data_budget: self.sender.data_budget.clone(),
            },
        )
    }
}

#[derive(Debug)]
pub(super) struct PumpDataReceiver {
    receiver: mpsc::Receiver<RelayOutbound>,
    data_budget: Arc<DataQueueByteBudget>,
}

impl PumpDataReceiver {
    pub(super) async fn recv(&mut self) -> Option<RelayOutbound> {
        let outbound = self.receiver.recv().await?;
        self.data_budget.release(outbound.queued_data_bytes());
        Some(outbound)
    }

    #[cfg(test)]
    pub(super) fn try_recv(
        &mut self,
    ) -> Result<RelayOutbound, tokio::sync::mpsc::error::TryRecvError> {
        let outbound = self.receiver.try_recv()?;
        self.data_budget.release(outbound.queued_data_bytes());
        Ok(outbound)
    }
}

#[derive(Debug)]
pub(super) enum RelayDataSendError {
    BudgetFull,
    Closed,
}

#[derive(Debug)]
pub(super) struct DataQueueByteBudget {
    limit: usize,
    queued: AtomicUsize,
    notify: Notify,
}

impl DataQueueByteBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            queued: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    pub(super) fn try_reserve(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }

        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.limit {
                return false;
            }
            match self.queued.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub(super) fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        // 中文注释：release 只在成功入队后的出队/发送失败回滚路径调用。
        // 使用 saturating_sub 兜住测试或未来改动造成的重复释放，不让计数下溢成巨大值。
        let _ = self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(bytes))
            });
        self.notify.notify_waiters();
    }

    pub(super) fn exceeds_limit(&self, bytes: usize) -> bool {
        bytes > self.limit
    }

    async fn reserve_or_wait(&self, bytes: usize, close_rx: &mut EndpointCloseReceiver) -> bool {
        if bytes == 0 {
            return true;
        }
        debug_assert!(!self.exceeds_limit(bytes));
        loop {
            // 中文注释：必须先注册/enable 通知，再检查容量。
            // 如果先 try_reserve 再 notified().await，release 可能刚好夹在两者之间，
            // notify_waiters 不会为未来 waiter 保留许可，等待方就会永久睡眠。
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_reserve(bytes) {
                return true;
            }
            tokio::select! {
                biased;

                _ = close_rx.closed() => return false,
                _ = &mut notified => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
struct EndpointCloseSignal {
    sender: watch::Sender<bool>,
}

impl EndpointCloseSignal {
    fn new() -> Self {
        let (sender, _receiver) = watch::channel(false);
        Self { sender }
    }

    fn subscribe(&self) -> EndpointCloseReceiver {
        EndpointCloseReceiver {
            receiver: self.sender.subscribe(),
        }
    }

    fn close(&self) {
        let _ = self.sender.send(true);
    }
}

#[derive(Debug)]
pub(super) struct EndpointCloseReceiver {
    receiver: watch::Receiver<bool>,
}

impl EndpointCloseReceiver {
    pub(super) async fn closed(&mut self) {
        if *self.receiver.borrow() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RelayOutbound {
    Frame(OpaqueFrame),
    Pong(Vec<u8>),
    Close,
}

impl RelayOutbound {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Frame(_) => "frame",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    pub(super) fn frame_kind(&self) -> &'static str {
        match self {
            Self::Frame(frame) => frame.kind(),
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    pub(super) fn queued_data_bytes(&self) -> usize {
        match self {
            Self::Frame(frame) => frame.len(),
            Self::Pong(_) | Self::Close => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedRelayOutbound {
    Frame(OpaqueFrame),
    Pong(Vec<u8>),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayWriteResult {
    Sent,
    Closed,
    Failed,
}

async fn send_relay_outbound(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
) -> RelayWriteResult {
    match prepare_relay_outbound(outbound) {
        PreparedRelayOutbound::Frame(frame) => {
            send_relay_opaque_frame(sender, server_id, role, connection_id, frame, channel).await
        }
        PreparedRelayOutbound::Pong(payload) => {
            match send_relay_established_message(
                sender,
                Message::Pong(payload),
                "relay websocket pong",
            )
            .await
            {
                Ok(()) => RelayWriteResult::Sent,
                Err(()) => RelayWriteResult::Failed,
            }
        }
        PreparedRelayOutbound::Close => {
            let _ = send_message_with_deadline(
                sender,
                Message::Close(None),
                WEBSOCKET_SEND_DEADLINE,
                "relay websocket close",
            )
            .await;
            RelayWriteResult::Closed
        }
    }
}

fn prepare_relay_outbound(outbound: RelayOutbound) -> PreparedRelayOutbound {
    match outbound {
        RelayOutbound::Frame(frame) => PreparedRelayOutbound::Frame(frame),
        RelayOutbound::Pong(payload) => PreparedRelayOutbound::Pong(payload),
        RelayOutbound::Close => PreparedRelayOutbound::Close,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_relay_websocket_writer(
    mut sender: futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    mut control_rx: mpsc::Receiver<RelayOutbound>,
    mut data_rx: mpsc::Receiver<RelayOutbound>,
    data_budget: Arc<DataQueueByteBudget>,
    mut close_rx: EndpointCloseReceiver,
    endpoint: FrameSender,
) {
    let mut prefer_data_once = false;
    let mut last_write_at = Instant::now();
    let mut idle_ping = tokio::time::interval_at(
        Instant::now() + WEBSOCKET_IDLE_PING_INTERVAL,
        WEBSOCKET_IDLE_PING_INTERVAL,
    );
    idle_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut idle_ping_nonce = 0_u64;
    loop {
        if prefer_data_once {
            tokio::select! {
                biased;

                _ = close_rx.closed() => {
                    write_relay_close_and_report(
                        &mut sender,
                    )
                    .await;
                    break;
                }
                outbound = data_rx.recv() => {
                    prefer_data_once = false;
                    let Some(outbound) = outbound else {
                        endpoint.close_endpoint();
                        break;
                    };
                    let outbound_label = outbound.label();
                    let frame_kind = outbound.frame_kind();
                    let queued_bytes = outbound.queued_data_bytes();
                    data_budget.release(queued_bytes);
                    trace!(
                        server_id = %server_id.0,
                        ?role,
                        connection_id,
                        channel = "data",
                        outbound = outbound_label,
                        frame_kind,
                        queued_bytes,
                        "relay websocket writer dequeued frame"
                    );
                    if !write_relay_outbound_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        outbound,
                        "data",
                    )
                    .await
                    {
                        endpoint.close_endpoint();
                        break;
                    }
                    last_write_at = Instant::now();
                }
                outbound = control_rx.recv() => {
                    let Some(outbound) = outbound else {
                        endpoint.close_endpoint();
                        break;
                    };
                    trace!(
                        server_id = %server_id.0,
                        ?role,
                        connection_id,
                        channel = "control",
                        outbound = outbound.label(),
                        frame_kind = outbound.frame_kind(),
                        queued_bytes = outbound.queued_data_bytes(),
                        "relay websocket writer dequeued frame"
                    );
                    prefer_data_once = true;
                    if !write_relay_outbound_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        outbound,
                        "control",
                    )
                    .await
                    {
                        endpoint.close_endpoint();
                        break;
                    }
                    last_write_at = Instant::now();
                }
                _ = idle_ping.tick() => {
                    if websocket_idle_ping_due(Instant::now(), last_write_at) {
                        if !write_relay_idle_ping_and_report(
                            &mut sender,
                            server_id,
                            role,
                            connection_id,
                            &mut idle_ping_nonce,
                        )
                        .await
                        {
                            endpoint.close_endpoint();
                            break;
                        }
                        last_write_at = Instant::now();
                    }
                }
            }
            continue;
        }

        tokio::select! {
            biased;

            _ = close_rx.closed() => {
                write_relay_close_and_report(
                    &mut sender,
                )
                .await;
                break;
            }
            outbound = control_rx.recv() => {
                let Some(outbound) = outbound else {
                    endpoint.close_endpoint();
                    break;
                };
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id,
                    channel = "control",
                    outbound = outbound.label(),
                    frame_kind = outbound.frame_kind(),
                    queued_bytes = outbound.queued_data_bytes(),
                    "relay websocket writer dequeued frame"
                );
                prefer_data_once = true;
                if !write_relay_outbound_and_report(
                    &mut sender,
                    server_id,
                    role,
                    connection_id,
                    outbound,
                    "control",
                )
                .await
                {
                    endpoint.close_endpoint();
                    break;
                }
                last_write_at = Instant::now();
            }
            outbound = data_rx.recv() => {
                prefer_data_once = false;
                let Some(outbound) = outbound else {
                    endpoint.close_endpoint();
                    break;
                };
                let outbound_label = outbound.label();
                let frame_kind = outbound.frame_kind();
                let queued_bytes = outbound.queued_data_bytes();
                data_budget.release(queued_bytes);
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id,
                    channel = "data",
                    outbound = outbound_label,
                    frame_kind,
                    queued_bytes,
                    "relay websocket writer dequeued frame"
                );
                if !write_relay_outbound_and_report(
                    &mut sender,
                    server_id,
                    role,
                    connection_id,
                    outbound,
                    "data",
                )
                .await
                {
                    endpoint.close_endpoint();
                    break;
                }
                last_write_at = Instant::now();
            }
            _ = idle_ping.tick() => {
                if websocket_idle_ping_due(Instant::now(), last_write_at) {
                    if !write_relay_idle_ping_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        &mut idle_ping_nonce,
                    )
                    .await
                    {
                        endpoint.close_endpoint();
                        break;
                    }
                    last_write_at = Instant::now();
                }
            }
        }
    }
}

async fn write_relay_close_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) {
    // 中文注释：这是独立于 mpsc 队列的关闭路径；队列满时也能尽力发送 close frame，
    // 并通知读侧退出。真正的 socket 回收由 handle_socket drop/abort 兜底。
    let _ = send_message_with_deadline(
        sender,
        Message::Close(None),
        WEBSOCKET_SEND_DEADLINE,
        "relay websocket close signal",
    )
    .await;
}

async fn write_relay_idle_ping_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    idle_ping_nonce: &mut u64,
) -> bool {
    *idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
    let payload = idle_ping_nonce.to_be_bytes().to_vec();
    // 中文注释：这是 WebSocket 控制帧保活，不进入 E2EE 业务协议。
    // relay 不等待业务 ACK，也不解析终端内容；ping 只用于让代理/NAT 看见连接活动。
    // 是否离线只能由底层 WebSocket read/write close/error 暴露，不能由 relay 自己计数裁定。
    match send_message_with_deadline(
        sender,
        Message::Ping(payload),
        WEBSOCKET_SEND_DEADLINE,
        "relay websocket idle ping",
    )
    .await
    {
        Ok(()) => {
            trace!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                "relay websocket idle ping sent"
            );
            true
        }
        Err(()) => {
            warn!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                "relay websocket idle ping failed"
            );
            false
        }
    }
}

async fn write_relay_outbound_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
) -> bool {
    match send_relay_outbound(sender, server_id, role, connection_id, outbound, channel).await {
        RelayWriteResult::Sent => true,
        RelayWriteResult::Closed => false,
        RelayWriteResult::Failed => false,
    }
}

async fn send_relay_opaque_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    frame: OpaqueFrame,
    channel: &'static str,
) -> RelayWriteResult {
    let frame_kind = frame.kind();
    let frame_len = frame.len();
    let started_at = Instant::now();
    if send_relay_established_message(sender, frame.into(), "relay websocket outbound frame")
        .await
        .is_err()
    {
        warn!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            frame_kind,
            frame_len,
            channel,
            "relay websocket send failed"
        );
        return RelayWriteResult::Failed;
    }
    let elapsed = started_at.elapsed();
    match websocket_outbound_frame_pressure_level(frame_len, elapsed) {
        OutboundFramePressureLevel::Info => {
            info!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                frame_kind,
                frame_len,
                channel,
                elapsed_ms = elapsed.as_millis(),
                "relay websocket outbound frame pressure"
            );
        }
        OutboundFramePressureLevel::Debug => {
            debug!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                frame_kind,
                frame_len,
                channel,
                elapsed_ms = elapsed.as_millis(),
                "relay websocket outbound large frame"
            );
        }
        OutboundFramePressureLevel::None => {}
    }
    RelayWriteResult::Sent
}

async fn send_relay_established_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
    context: &'static str,
) -> Result<(), ()> {
    // relay 建连之后就是 dumb pipe：慢网络只应该形成 WebSocket/TCP 背压，
    // 但 established frame 仍然必须有 deadline，避免 browser 端半开写入把下行永远卡住。
    send_message_with_deadline(sender, message, WEBSOCKET_SEND_DEADLINE, context).await
}
