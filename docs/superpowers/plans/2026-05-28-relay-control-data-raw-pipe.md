# Relay Control/Data Raw Pipe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 `termrelay` 收敛为 daemon control 长连接加每个 browser client 独立 daemon data 反连的 raw pipe 模型。

**Architecture:** relay 只读取第一帧 `route_hello` 做 `server_id` 和 route role 路由。`DaemonControl` 只承载 `RelayControlEnvelope::OpenData` 和 `ClientDisconnected` 生命周期消息；`DaemonData` 与 `Client` 一一配对后只原样转发 text/binary WebSocket frame。旧 `DaemonMux` 只能作为协议兼容枚举存在，relay runtime 必须拒绝它，不能解析或封装 mux 业务报文。

**Tech Stack:** Rust、Axum WebSocket、tokio mpsc/watch、tokio-tungstenite relay tests、Cargo workspace tests。

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

### Task 1: Clean relay runtime mux remnants

**Files:**
- Modify: `termrelay/src/ws.rs`

- [x] **Step 1: Remove old client mux branching from `handle_socket`**

Expected shape:

```rust
if role == ConnectionRole::Client {
    let Some(mut pair_rx) = state.client_pair_receiver(&registration) else {
        state.unregister(&registration);
        let _ = send_route_error(&mut socket, "relay_data_route_invalid", "...").await;
        return;
    };
    if timeout(ROUTE_PRELUDE_TIMEOUT, pair_rx.closed()).await.is_err()
        || !state.client_has_data_pair(&registration)
    {
        state.unregister(&registration);
        let _ = send_route_error(&mut socket, "relay_data_route_timeout", "...").await;
        return;
    }
}
```

- [x] **Step 2: Remove mux outbound preparation**

Expected shape:

```rust
fn prepare_relay_outbound(outbound: RelayOutbound) -> PreparedRelayOutbound {
    match outbound {
        RelayOutbound::Frame(frame) => PreparedRelayOutbound::Frame(frame),
        RelayOutbound::Pong(payload) => PreparedRelayOutbound::Pong(payload),
        RelayOutbound::Close => PreparedRelayOutbound::Close,
    }
}
```

- [x] **Step 3: Remove route generation and mux helper functions**

Delete old helpers that mention `RelayMuxEnvelope`, `RelayOpaqueFrame`, `mux_envelope_*`, `notify_mux_client_connected`, `DaemonMuxBusy`, `DaemonMuxOffline`, or `ConnectionRole::DaemonMux`.

- [x] **Step 4: Run focused compile**

Run:

```bash
cargo check -p termrelay --locked
```

Expected: compile succeeds, or remaining failures are only old tests that Task 2 rewrites.

### Task 2: Rewrite relay tests for control/data route model

**Files:**
- Modify: `termrelay/src/router.rs`
- Modify: `termrelay/src/ws.rs`
- Modify: `termrelay/tests/relay_e2e.rs` only if compile failures require it

- [x] **Step 1: Replace router mux tests with control/data raw pipe tests**

Test flow:

```rust
DaemonControl route_ready
Client sends route_hello and waits
DaemonControl receives RelayControlEnvelope::OpenData
DaemonData connects with client_id/data_token
DaemonData route_ready
Client route_ready
Client text/binary is received unchanged by DaemonData
DaemonData text/binary is received unchanged by Client
```

- [x] **Step 2: Replace ws unit tests with new lifecycle invariants**

Cover:

```text
legacy DaemonMux route is rejected
client waits for daemon data pair before route_ready
control disconnect closes clients and data pipes
client disconnect notifies control and closes paired data
daemon data disconnect closes only paired client
slow client data queue closes that client/data without closing control
```

- [x] **Step 3: Remove all test references to mux helpers**

Run:

```bash
rg -n "DaemonMux|RelayMuxEnvelope|RelayOpaqueFrame|decode_binary_relay_mux_envelope|encode_binary_relay_mux_envelope|MuxClientFrame|daemon_mux|mux_envelope|notify_mux" termrelay/src/ws.rs termrelay/src/router.rs
```

Expected: only the explicit legacy rejection test or no matches in relay runtime/tests.

### Task 3: Full verification and architecture review

**Files:**
- Verify: `termrelay/src/ws.rs`
- Verify: `termrelay/src/router.rs`
- Verify: `termrelay/tests/relay_e2e.rs`

- [x] **Step 1: Format and whitespace checks**

Run:

```bash
cargo fmt --all --check
git diff --check -- termrelay/src/ws.rs termrelay/src/router.rs termrelay/tests/relay_e2e.rs proto/src/lib.rs termd/src/net/relay.rs
```

Expected: both commands exit 0.

- [x] **Step 2: Rust test suite**

Run:

```bash
cargo test -p termrelay --locked
cargo test -p termd --bin termd --locked
cargo test --workspace --locked
```

Expected: all commands exit 0.

- [x] **Step 3: Manual复审**

Checklist:

```text
relay runtime rejects RouteRole::DaemonMux
relay runtime does not import or parse RelayMuxEnvelope
client route_ready is sent only after daemon data pair exists
data path forwards raw text/binary frames
control path only carries OpenData/ClientDisconnected
client/data/control disconnect scopes close the correct transports
relay does not parse terminal or E2EE business payloads
```
