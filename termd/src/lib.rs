//! termd daemon crate 的内核集成入口。
//!
//! 当前公开 PTY、session/control 和设备级 auth 基础模块。WebSocket、relay
//! 和持久化会在后续优先级中接入，避免把 daemon 过早做成复杂平台。

pub mod auth;
pub mod config;
pub mod net;
pub mod pty;
pub mod runtime;
pub mod session;
mod session_ownership;
pub mod state;
