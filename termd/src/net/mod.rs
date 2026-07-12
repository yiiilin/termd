//! termd 的 HTTP/WebSocket 网络边界。
//!
//! JSON HTTP 负责认证和控制操作，metadata/terminal WebSocket 负责工作区实时流量。
//! trusted relay 可以看到 TLS 终止后的明文应用流量；pairing、challenge-response、
//! operator 和 session 权限仍由 daemon 校验。

pub mod protocol;
pub mod pty_bridge;
pub mod relay;
pub(crate) mod screen;
pub mod server;
pub mod signature;
