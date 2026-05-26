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

- [x] Codify that a `session.list` UI timeout does not close the underlying `DirectClient`, and late responses do not block later RPC.
- [x] Write a failing test proving `terminal.attach` can use a longer stream timeout than ordinary RPC.
- [x] Add per-call stream timeout support to `DirectClient.attachSession`.
- [x] Keep hard timeout behavior for connect, route prelude, E2EE handshake, pairing, and auth challenge.
- [x] Run `npm test -- --run src/__tests__/direct-client.test.ts`.

## Task 2: App State Isolation For Slow Non-Terminal RPC

**Files:**
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`
- Modify: `termui/frontend/src/test/mock-daemon.ts`

- [x] Add tests where daemon status / clients / files RPC exceeds `APP_CONNECTION_TIMEOUT_MS` while terminal remains attached.
- [x] Ensure non-terminal timeout sets only the relevant panel error/stale state.
- [x] Ensure workspace surface and xterm remain mounted when terminal stream is healthy.
- [x] Ensure late responses apply only when active daemon/session generation still matches.
- [x] Run `npm test -- --run src/__tests__/app.test.tsx`.
- [x] Mark this task complete only after the tests pass.

## Task 3: Daemon Direct Backpressure Hardening

**Files:**
- Modify: `termd/src/net/server.rs`
- Modify: related tests in `termd/src/net/server.rs`

- [x] Add tests proving writer queue capacity waits do not use short network timeouts for terminal output.
- [x] Audit `enqueue_websocket_wire`, `enqueue_websocket_control_raw`, and push drain paths for timeout-as-failure behavior.
- [x] Keep route prelude timeout as a setup-only hard timeout.
- [x] Ensure writer send failure is still propagated to close that browser connection.
- [x] Run `cargo test -p termd websocket_push -- --nocapture`.
- [x] Mark this task complete only after the tests pass.

## Task 4: Relay Dumb Pipe Resilience

**Files:**
- Modify: `termrelay/src/ws.rs`
- Modify: `termrelay/tests/relay_e2e.rs`

- [x] Add tests for slow relay writer queue behavior where data waits for capacity instead of business-failing.
- [x] Ensure relay does not classify ordinary payload delay as daemon offline.
- [x] Keep route prelude timeout only for clients that never identify their route.
- [x] Ensure daemon offline is tied to daemon WebSocket close/error.
- [x] Run `cargo test -p termrelay data_queue queued_client_frame -- --nocapture` or split equivalent single-filter commands.
- [x] Mark this task complete only after the tests pass.

## Task 5: Poor-Network End-To-End Coverage

**Files:**
- Modify: `termui/frontend/tests/termui-web.real-relay.spec.ts`
- Modify or add: Rust/TypeScript test helpers as needed.

- [x] Add a test profile with high RTT, delayed responses, and slow non-terminal RPC.
- [x] Cover direct mode: terminal remains usable when daemon.status/session.files times out.
- [x] Cover relay mode: attach, input echo, resize, and session switch recover after delayed responses.
- [x] Cover browser hidden/visible with stale request cleanup.
- [x] Run frontend unit tests, targeted Rust tests, and any real-relay Playwright test available in the local environment.
- [x] Mark this task complete only after verification passes or document unavailable external dependency explicitly.

## Task 6: Final Verification And Commit

**Files:**
- All modified source and test files.

- [x] Run `cargo fmt --check`.
- [x] Run `cargo check -p termd -p termrelay`.
- [x] Run targeted Rust tests for websocket push and relay queue behavior.
- [x] Run `npm test -- --run`.
- [x] Run `npm run build`.
- [x] Commit the implementation with a clear message.
