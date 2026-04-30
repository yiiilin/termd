//! termrelay 的 HTTP/WebSocket 入口。
//!
//! relay 只负责按 URL 中公开的 `server_id` 转发 WebSocket frame。它不解密、不解析
//! 内层业务 envelope，也不参与 pairing/auth/session/control 权限判断。

mod args;
mod router;
mod ws;

use std::net::SocketAddr;

use thiserror::Error;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::args::{Args, ArgsError};
use crate::router::router;
use crate::ws::RelayState;

#[derive(Debug, Error)]
enum MainError {
    #[error(transparent)]
    Args(#[from] ArgsError),
    #[error("failed to bind relay HTTP listener at {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("relay HTTP server failed")]
    Serve(#[source] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), MainError> {
    init_tracing();

    let args = Args::from_env()?;
    let listener = TcpListener::bind(args.listen)
        .await
        .map_err(|source| MainError::Bind {
            addr: args.listen,
            source,
        })?;

    info!(listen = %args.listen, "starting termrelay dumb pipe");

    axum::serve(listener, router(RelayState::default()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(MainError::Serve)
}

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("termrelay=info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    // 监听 Ctrl-C 即可满足 MVP；systemd/k8s 等更复杂生命周期不进入本轮 relay。
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for shutdown signal");
    }
}
