use std::time::Duration;

use axum::extract::ws::Message;
use futures_util::{Sink, SinkExt as _};
use termd_proto::ServerId;
use tokio::time::{Instant, timeout};
use tracing::{debug, warn};

use super::{ConnectionId, ConnectionRole};

// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
pub(super) const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
pub(super) const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
pub(super) const WEBSOCKET_OUTBOUND_FRAME_PRESSURE_INFO_THRESHOLD: Duration =
    Duration::from_millis(50);
pub(super) const WEBSOCKET_OUTBOUND_FRAME_PRESSURE_DEBUG_BYTES: usize = 128 * 1024;
#[cfg(not(test))]
pub(super) const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(super) const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_millis(50);
// 终端 snapshot 是 E2EE 后的 opaque binary frame，relay 不能拆包或解析。
// 这里的上限必须能容纳 1000 行 scrollback 的完整重绘，同时仍保留传输层内存保护。
pub(crate) const WEBSOCKET_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub(crate) const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutboundFramePressureLevel {
    None,
    Debug,
    Info,
}

pub(super) fn websocket_outbound_frame_pressure_level(
    frame_len: usize,
    elapsed: Duration,
) -> OutboundFramePressureLevel {
    if elapsed >= WEBSOCKET_OUTBOUND_FRAME_PRESSURE_INFO_THRESHOLD {
        return OutboundFramePressureLevel::Info;
    }
    if frame_len >= WEBSOCKET_OUTBOUND_FRAME_PRESSURE_DEBUG_BYTES {
        return OutboundFramePressureLevel::Debug;
    }
    OutboundFramePressureLevel::None
}

pub(super) fn websocket_idle_ping_due(now: Instant, last_write_at: Instant) -> bool {
    now.duration_since(last_write_at) >= WEBSOCKET_IDLE_PING_INTERVAL
}

#[derive(Debug, Clone, Copy)]
pub(super) struct WebSocketReceiveDebug {
    pub(super) last_inbound_at: Instant,
    pub(super) last_inbound_kind: &'static str,
    pub(super) inbound_messages: u64,
    pub(super) inbound_bytes: u64,
}

impl WebSocketReceiveDebug {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            last_inbound_at: now,
            last_inbound_kind: "none",
            inbound_messages: 0,
            inbound_bytes: 0,
        }
    }

    pub(super) fn record(&mut self, kind: &'static str, bytes: usize) {
        let now = Instant::now();
        self.last_inbound_at = now;
        self.last_inbound_kind = kind;
        self.inbound_messages = self.inbound_messages.saturating_add(1);
        self.inbound_bytes = self.inbound_bytes.saturating_add(bytes as u64);
    }
}

pub(super) fn websocket_message_kind(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
    }
}

pub(super) fn websocket_message_bytes(message: &Message) -> usize {
    match message {
        Message::Text(raw) => raw.len(),
        Message::Binary(raw) => raw.len(),
        Message::Ping(payload) | Message::Pong(payload) => payload.len(),
        Message::Close(_) => 0,
    }
}

pub(super) fn log_websocket_receive_failed(
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    error: &axum::Error,
    receive_debug: &WebSocketReceiveDebug,
) {
    let error_text = error.to_string();
    if websocket_receive_failed_is_noisy_client_disconnect(role, &error_text) {
        debug!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            ?receive_debug,
            "relay websocket receive failed"
        );
    } else {
        warn!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            ?receive_debug,
            "relay websocket receive failed"
        );
    }
}

pub(super) fn websocket_receive_failed_is_noisy_client_disconnect(
    role: ConnectionRole,
    error_text: &str,
) -> bool {
    role == ConnectionRole::Client
        && error_text.contains("Connection reset without closing handshake")
}

pub(super) fn reject_oversized_frame(len: usize) -> Result<(), usize> {
    // axum 的升级配置在 router 层；这里在 ws 层再做一次元数据大小闸门，避免继续转发超限 frame。
    let max = WEBSOCKET_MAX_FRAME_SIZE.min(WEBSOCKET_MAX_MESSAGE_SIZE);
    if len > max { Err(len) } else { Ok(()) }
}

pub(super) async fn send_message_with_deadline<S>(
    sender: &mut S,
    message: Message,
    deadline: Duration,
    context: &'static str,
) -> Result<(), ()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    match timeout(deadline, sender.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            warn!(error = ?error, context = context, "relay websocket send failed");
            Err(())
        }
        Err(_) => {
            warn!(
                ?deadline,
                context = context,
                "relay websocket send timed out"
            );
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::Message;
    use futures_util::Sink;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct NeverReadySink;

    impl Sink<Message> for NeverReadySink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            unreachable!("poll_ready never resolves")
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct FlushStallingSink {
        start_send_called: bool,
    }

    impl Sink<Message> for FlushStallingSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            self.get_mut().start_send_called = true;
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn send_message_with_deadline_times_out_when_sink_stalls() {
        let mut sink = NeverReadySink;
        let result = send_message_with_deadline(
            &mut sink,
            Message::Text("stall".to_owned()),
            Duration::from_millis(20),
            "test websocket stall",
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_message_with_deadline_times_out_when_flush_stalls() {
        let mut sink = FlushStallingSink {
            start_send_called: false,
        };
        let result = send_message_with_deadline(
            &mut sink,
            Message::Text("flush-stall".to_owned()),
            Duration::from_millis(20),
            "test websocket flush stall",
        )
        .await;

        assert!(sink.start_send_called);
        assert!(result.is_err());
    }
}
