# Network Resilience Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the weak-timeout, strong-recovery, terminal-first network model for direct and relay usage.

**Architecture:** WebSocket close/error remains the only connection-failure signal after setup. Ordinary RPC deadlines become UI deadlines with stale response handling. Terminal streams keep priority and recover through reconnect + snapshot + tail.

**Tech Stack:** Rust (`tokio`, `axum`, `tokio-tungstenite`), TypeScript/React, Vitest, existing termd protocol tests.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Task 1: Frontend Soft Timeout Semantics

**Files:**
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Modify: `termui/frontend/src/__tests__/direct-client.test.ts`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`

- [ ] Write failing tests showing that a `session.list` or `daemon.status` request can hit a UI timeout without closing the underlying `DirectClient`.
- [ ] Add stale request handling to `DirectClient`: timed-out requests reject their waiter but keep a stale record so late responses are ignored safely.
- [ ] Keep hard timeout behavior for connect, route prelude, E2EE handshake, pairing, and auth challenge.
- [ ] Run `npm test -- --run src/__tests__/direct-client.test.ts src/__tests__/app.test.tsx`.
- [ ] Mark this task complete only after the tests pass.

## Task 2: App State Isolation For Slow Non-Terminal RPC

**Files:**
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`
- Modify: `termui/frontend/src/test/mock-daemon.ts`

- [ ] Add tests where daemon status / clients / files RPC exceeds `APP_CONNECTION_TIMEOUT_MS` while terminal remains attached.
- [ ] Ensure non-terminal timeout sets only the relevant panel error/stale state.
- [ ] Ensure workspace surface and xterm remain mounted when terminal stream is healthy.
- [ ] Ensure late responses apply only when active daemon/session generation still matches.
- [ ] Run `npm test -- --run src/__tests__/app.test.tsx`.
- [ ] Mark this task complete only after the tests pass.

## Task 3: Daemon Direct Backpressure Hardening

**Files:**
- Modify: `termd/src/net/server.rs`
- Modify: related tests in `termd/src/net/server.rs`

- [ ] Add tests proving writer queue capacity waits do not use short network timeouts for terminal output.
- [ ] Audit `enqueue_websocket_wire`, `enqueue_websocket_control_raw`, and push drain paths for timeout-as-failure behavior.
- [ ] Keep route prelude timeout as a setup-only hard timeout.
- [ ] Ensure writer send failure is still propagated to close that browser connection.
- [ ] Run `cargo test -p termd websocket_push -- --nocapture`.
- [ ] Mark this task complete only after the tests pass.

## Task 4: Relay Dumb Pipe Resilience

**Files:**
- Modify: `termrelay/src/ws.rs`
- Modify: `termrelay/tests/relay_e2e.rs`

- [ ] Add tests for slow relay writer queue behavior where data waits for capacity instead of business-failing.
- [ ] Ensure relay does not classify ordinary payload delay as daemon offline.
- [ ] Keep route prelude timeout only for clients that never identify their route.
- [ ] Ensure daemon offline is tied to daemon WebSocket close/error.
- [ ] Run `cargo test -p termrelay data_queue queued_client_frame -- --nocapture` or split equivalent single-filter commands.
- [ ] Mark this task complete only after the tests pass.

## Task 5: Poor-Network End-To-End Coverage

**Files:**
- Modify: `termui/frontend/tests/termui-web.real-relay.spec.ts`
- Modify or add: Rust/TypeScript test helpers as needed.

- [ ] Add a test profile with high RTT, delayed responses, and slow non-terminal RPC.
- [ ] Cover direct mode: terminal remains usable when daemon.status/session.files times out.
- [ ] Cover relay mode: attach, input echo, resize, and session switch recover after delayed responses.
- [ ] Cover browser hidden/visible with stale request cleanup.
- [ ] Run frontend unit tests, targeted Rust tests, and any real-relay Playwright test available in the local environment.
- [ ] Mark this task complete only after verification passes or document unavailable external dependency explicitly.

## Task 6: Final Verification And Commit

**Files:**
- All modified source and test files.

- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo check -p termd -p termrelay`.
- [ ] Run targeted Rust tests for websocket push and relay queue behavior.
- [ ] Run `npm test -- --run`.
- [ ] Run `npm run build`.
- [ ] Commit the implementation with a clear message.

