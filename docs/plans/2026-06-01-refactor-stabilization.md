# Refactor Stabilization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复扫描中发现的关键不变量缺口，并把 daemon protocol、relay、CLI、Web 和发布流程逐步收敛到更小、更稳定、更容易验证的边界。

**Architecture:** 先修会直接破坏安全/部署/relay 边界的 P0，再抽取重复 guard 和共享协议映射。Relay 继续保持 dumb pipe；daemon 仍是 auth/session 的唯一权威；UI 和 CLI 只增强本地状态机、请求防过期和恢复能力。

**Tech Stack:** Rust workspace (`termd`, `termctl`, `termrelay`, `proto`, `termweb`), TypeScript/React/Vitest/Playwright, Flutter native skeleton, shell release scripts, GitHub Actions.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Scope And Order

- P0 tasks must be completed before broad refactors.
- Tasks 3 through 8 touch mostly disjoint areas and may be handled by separate subagents after Task 1 and Task 2 land.
- Do not change relay into a business-aware component. Relay may route transport metadata, but it must not parse auth/session/control/session data semantics.
- Prefer small extraction commits over semantic rewrites. Each task should add focused regression tests before implementation.

## Task 1: Enforce Session Attach Invariant For Close

**Files:**
- Modify: `termd/src/net/protocol.rs`

- [x] Add a regression test near the existing unattached-operation invariant tests proving an authenticated but unattached connection cannot close a session.
- [x] In the test, create a session with one attached connection, authenticate a second connection without attaching it, send `session.close` from the unattached connection, and assert the response is `invalid_state`.
- [x] In the same test, assert the backing session remains running and the mock/session termination counter does not increase.
- [x] Update `close_session` to require `connection.ensure_attached_to(payload.session_id)?` before resolving or terminating the session.
- [x] Update `termctl close` to attach the target session before sending close, and cover the CLI close compatibility path with a focused regression test.
- [x] Run `cargo test -p termd unattached -- --nocapture`.
- [x] Run `cargo test -p termd close_session -- --nocapture`.

## Task 2: Fix Termrelay Deploy Smoke Breakage

**Files:**
- Modify: `deploy/termrelay/docker-compose.yml`
- Modify if needed: `deploy/termrelay/.env.example`
- Modify if needed: `deploy/termrelay/Caddyfile`
- Modify if needed: `docs/deployment.md`
- Modify if needed: `termrelay/src/args.rs`
- Modify if needed: `termrelay/src/main.rs`

- [x] Change the compose service so it does not pass `/bin/sh -ec ...` to the `scratch` image entrypoint.
- [x] Keep production deployment requiring an explicit non-empty relay token via Docker secret file and `--auth-token-file`; if a no-auth local dev mode is still needed, document it separately from the public Caddy deployment.
- [x] Ensure Caddy logs do not leak `relay_token` query parameters on upstream errors or access logs; document the log redaction behavior.
- [x] Avoid exposing production relay tokens in Docker argv or resolved compose config by supporting a secret-file path such as `termrelay --auth-token-file /run/secrets/termrelay_auth_token` and wiring compose to a Docker secret.
- [x] Update `.env.example` so the image tag is either current or clearly a placeholder, not stale `0.1.0`.
- [x] Run `docker compose -f deploy/termrelay/docker-compose.yml config`.
- [x] If Docker is available, run a short container startup smoke for `termrelay --help` or equivalent non-network command.

## Task 3: Decide And Isolate Relay HTTP File Tunnel Boundary

**Files:**
- Modify: `termrelay/src/router.rs`
- Modify: `termrelay/src/ws.rs`
- Modify: `termrelay/tests/relay_e2e.rs`
- Modify if needed: `docs/deployment.md`

- [x] Add a relay test documenting the desired boundary: WebSocket opaque frames must still pass without relay parsing business message types.
- [x] Choose the short-term behavior for `/api/files/upload*` and `/api/files/download`: either disable behind an explicit config flag, or convert path-specific handling into a generic opaque tunnel path.
- [x] Remove path-specific timeout/deadline behavior from relay tunnel code unless the chosen config explicitly enables the compatibility path.
- [x] Ensure relay never decrypts or interprets session/auth/control payloads while forwarding file transfer traffic.
- [x] Run `cargo test -p termrelay router -- --nocapture`.
- [x] Run `cargo test -p termrelay --test relay_e2e -- --nocapture`.

## Task 4: Centralize Daemon Session-Scoped Guards

**Files:**
- Modify: `termd/src/net/protocol.rs`

- [x] Introduce a small internal helper such as `require_attached_session(...)` that authenticates the connection, checks attach state, and resolves the internal session id/root context needed by handlers.
- [x] Convert `write_session_data`, `resize_session`, `record_session_cursor`, `rename_session`, `close_session`, file operations, and Git/session-scoped operations to use the helper.
- [x] Keep error codes compatible with existing tests unless a test currently encodes an unsafe behavior.
- [x] Add a table-style invariant test for unauthenticated, authenticated-unattached, attached-wrong-session, and attached-correct-session cases across mutating handlers.
- [x] Run `cargo test -p termd session_scoped -- --nocapture`.
- [x] Run `cargo test -p termd --lib`.

## Task 5: Harden Auth Token, Challenge, And Replay Lifecycle

**Files:**
- Modify: `termd/src/auth/mod.rs`
- Modify: `termd/src/net/protocol.rs`

- [x] Add tests proving expired pairing tokens are pruned on token issue and token consume paths.
- [x] Add tests proving expired or consumed auth challenges are pruned on challenge issue and auth verification paths.
- [x] Add a per-device outstanding challenge cap or equivalent bounded cleanup behavior.
- [x] Align WebSocket auth replay nonce handling with the HTTP E2EE path: check first, verify signature, then record the nonce.
- [x] Run `cargo test -p termd auth -- --nocapture`.
- [x] Run `cargo test -p termd replay -- --nocapture`.

## Task 6: Make Terminal Stream Replacement Failure-Safe

**Files:**
- Modify: `termd/src/net/protocol.rs`

- [x] Add a test where an existing terminal stream is active and a malformed packet stream-open request fails.
- [x] Assert the old stream remains registered and still receives output/deferred wakeups after the failed request.
- [x] Change packet terminal stream attach/create so it decodes and validates the new stream before clearing or replacing existing stream state.
- [x] If rollback is simpler than delayed replacement, implement rollback and cover it in the test.
- [x] Run `cargo test -p termd terminal_stream -- --nocapture`.

## Task 7: Share Packet Codec And Protocol Method Mapping

**Files:**
- Modify: `proto/src`
- Modify: `termd/src/net/protocol.rs`
- Modify: `termctl/src/client.rs`
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Modify: `termui/frontend/src/test/mock-daemon.ts`
- Add if needed: `termui/frontend/src/protocol/packet-codec.ts`
- Add if needed: `termui/frontend/src/protocol/methods.ts`

- [x] In Rust, move method constants and JSON/binary packet conversion primitives into `proto` or another existing shared protocol module.
- [x] Replace duplicate method strings in `termctl` and daemon dispatch with shared constants where practical.
- [x] In TypeScript, move `protocolPacketToBinary` and `binaryPacketToProtocol` out of `direct-client.ts` and mock daemon into a shared codec module.
- [x] Add codec round-trip tests for request, response, event, stream-open, stream-data, stream-close, and terminal frame payloads.
- [x] Add a small protocol method registry for `method -> legacy envelope type` and `event method -> envelope type` mappings.
- [x] Run `cargo test -p termd-proto`.
- [x] Run `cargo build -p termd --bin termd && cargo test -p termctl`.
- [x] Run `cd termui/frontend && npm test -- --run src/__tests__/packet-codec.test.ts src/__tests__/direct-client.test.ts`.

## Task 8: Add DirectClient Phase Guards And Error Delivery

**Files:**
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Modify: `termui/frontend/src/__tests__/direct-client.test.ts`
- Modify: `termui/frontend/src/test/mock-daemon.ts`

- [x] Add tests proving `pair`, `authenticate`, `listSessions`, `attachSession`, and terminal stream operations fail locally with clear errors when called in the wrong phase.
- [x] Add a test where daemon sends a packet error with no request `id` and no `stream_id`; assert it reaches the UI-facing error queue instead of being dropped.
- [x] Add internal phases such as `connecting`, `e2ee_ready`, `authenticated`, `terminal_stream_open`, and `closed` without changing the public `DirectClient` API.
- [x] Add `requireE2eeReady`, `requireAuthenticated`, and `requireTerminalStream` helpers.
- [x] Update `dispatchPacketError` so unowned errors become ordinary error envelopes or close-level errors visible to callers.
- [x] Run `cd termui/frontend && npm test -- --run src/__tests__/direct-client.test.ts`.
- [x] Run `cd termui/frontend && npm run typecheck`.

## Task 9: Extract Frontend Connection And Terminal Hooks

**Files:**
- Modify: `termui/frontend/src/App.tsx`
- Add if needed: `termui/frontend/src/hooks/useWorkspaceConnection.ts`
- Add if needed: `termui/frontend/src/hooks/useTerminalAttach.ts`
- Add if needed: `termui/frontend/src/hooks/useSessionFiles.ts`
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Add if needed: `termui/frontend/src/components/terminal/useTerminalOutputWriter.ts`
- Add if needed: `termui/frontend/src/components/terminal/useTerminalFocusResize.ts`

- [x] Extract workspace connection/reconnect timers from `App.tsx` into a hook or reducer with explicit states.
- [x] Extract terminal attach and stream lifecycle into a separate hook that owns attach generation, reconnect scheduling, and terminal sequence tracking.
- [x] Extract session file loading, cwd following, upload/download progress filtering, and file panel errors into a separate hook.
- [x] Extract terminal output writer behavior from `TerminalPane` and add a pending-byte high-water behavior or explicit resync fallback.
- [x] Extract focus/resize/global listener handling from `TerminalPane`.
- [x] Run `cd termui/frontend && npm test -- --run src/__tests__/app.test.tsx`.
- [x] Run `cd termui/frontend && npm run typecheck`.

## Task 10: Fix Frontend Request Races And UI Drift

**Files:**
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/components/FileEditorDialog.tsx`
- Modify: `termui/frontend/src/components/SessionFilesPanel.tsx`
- Modify: `termui/frontend/src/components/SessionList.tsx`
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Modify: `termui/frontend/src/__tests__`

- [x] Add `requestId/sessionId/path` guards for file open and Git diff open flows so stale responses cannot overwrite the current dialog.
- [x] Disable backdrop close while file save is in progress, or pass a single `canClose` flag that controls every close path.
- [x] Filter upload/download progress by the currently attached session before passing it to `SessionFilesPanel`.
- [x] Add `draftDirty` or focus guard so `pathDraft` is not overwritten by cwd polling while the user is typing.
- [x] Change `SessionList` so the row is not a `div role="button"` containing nested real buttons; make the primary open affordance a real button.
- [x] Add request sequencing for terminal search so old search results cannot overwrite newer queries.
- [x] Run `cd termui/frontend && npm test -- --run`.
- [x] Run `cd termui/frontend && npm run test:e2e -- --project=chromium tests/termui-web.smoke.spec.ts` if browser dependencies are available.

## Task 11: Stabilize Termctl State And Attach UX

**Files:**
- Modify: `termctl/src/state.rs`
- Modify: `termctl/src/cli.rs`
- Modify: `termctl/src/client.rs`
- Modify: `termctl/src/error.rs`
- Modify: `termctl/tests/direct_daemon_e2e.rs`
- Add if needed: focused unit tests under `termctl/tests`

- [x] Replace direct truncate/write state persistence with `0600` temporary file write, flush/fsync, and atomic rename.
- [x] Persist generated device identity before sending pair acceptance to daemon; surface a specific error if final save fails.
- [x] Add `termctl pair <invite_or_token>` while preserving existing flags for compatibility.
- [x] Add better invite errors such as invalid invite, expired invite, missing URL, and token requires known daemon.
- [x] Add `watch_updates` control to attach RPC so short commands like `control` and `resize` do not subscribe to terminal output.
- [x] Implement attach raw mode guard, initial TTY resize, SIGWINCH resize, terminal sequence tracking, and reconnect/resume.
- [x] Add global `--json` for scriptable output while preserving current human output.
- [x] Run `cargo test -p termctl`.
- [x] Run `cargo test -p termctl --test direct_daemon_e2e -- --nocapture`.

## Task 12: Add Relay Resource Bounds

**Files:**
- Modify: `termrelay/src/ws.rs`
- Modify: `termrelay/tests/relay_e2e.rs`

- [x] Add a pending client pair deadline after client registration when daemon data pairing does not arrive.
- [x] Add per-room pending client count and total pre-pair byte limits.
- [x] Add per-room idle daemon data pipe limit; reject or close the oldest idle pipe when exceeded.
- [x] Use constant-time comparison for relay auth token and reject clearly unsafe production token lengths if compatible with existing CLI/docs.
- [x] Add tests for pending client timeout, pending byte cap, idle data pipe cap, and auth token behavior.
- [x] Run `cargo test -p termrelay --test relay_e2e -- --nocapture`.
- [x] Run `cargo test -p termrelay`.

## Task 13: Align QA, Release, And CI

**Files:**
- Modify: `scripts/qa.sh`
- Modify: `scripts/prepare-release.sh`
- Modify: `scripts/release-notes.sh`
- Modify: `.github/workflows/release.yml`
- Add: `.github/workflows/ci.yml`
- Modify: `docs/qa.md`
- Modify if needed: `TECH.md`

- [x] Add PR/push CI that runs Rust fmt/tests with `--locked`, frontend typecheck/test/build, and shell syntax checks.
- [x] Make local QA use `cargo test --workspace --locked`.
- [x] Make frontend QA run `npm ci` by default; allow skipping only through an explicit environment variable documented in `docs/qa.md`.
- [x] Make `prepare-release.sh` check for a clean worktree before version changes, with an explicit opt-in escape hatch if needed.
- [x] Make release validation reuse `scripts/qa.sh` or call the same Rust/Web/Native job pieces with the same flags.
- [x] Extend version checks to `termui/frontend/package.json` and `package-lock.json`.
- [x] Prevent GitHub Release creation when release notes still contain the placeholder text.
- [x] Update `TECH.md` so it includes `termweb` in the workspace overview.
- [x] Run `bash -n scripts/*.sh`.
- [x] Run `bash scripts/qa.sh` or document unavailable external dependencies explicitly.

## Task 14: Harden Native Storage And Parsing Boundaries

**Files:**
- Modify: `termui/native/lib/core/device/device_key_manager.dart`
- Modify: `termui/native/lib/core/device/paired_server.dart`
- Modify: `termui/native/lib/core/services/termui_native_service.dart`
- Modify: `termui/native/lib/core/protocol/protocol_client.dart`
- Modify: `termui/native/lib/core/errors/native_error.dart`
- Add or modify: `termui/native/test/support`
- Modify: `termui/native/test/core`

- [x] Store native device identity as one versioned record, or add a storage abstraction that writes the identity bundle atomically.
- [x] Make paired server parsing fail closed: malformed JSON, missing fields, non-map list entries, and duplicate IDs should become `NativeStateCorrupted` or another typed native error.
- [x] Validate pairing URL with `Uri.tryParse`, only allow `ws` and `wss`, require a host, and persist the normalized URL.
- [x] Add `serverId` or `PairedServerRef` scope to native protocol client methods, or explicitly enforce single-daemon mode in the store layer.
- [x] Move repeated fake secure storage fixtures into `termui/native/test/support`.
- [x] Add tests for malformed paired server state, duplicate server IDs, URL validation, and sensitive error redaction.
- [x] Run `cd termui/native && flutter test` if Flutter is available.
- [x] Run `cd termui/native && flutter analyze` if Flutter is available.

## Task 15: Final Integration Verification

**Files:**
- Review all modified files.

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --locked`.
- [x] Run `bash scripts/test-installers.sh`.
- [x] Run `cd termui/frontend && npm ci && npm run typecheck && npm test -- --run && npm run build`.
- [x] Run frontend Playwright e2e tests that are available in the local browser environment.
- [x] Run native Flutter verification if the toolchain is available; otherwise record the explicit skip reason in the final status.
- [x] Run `docker compose -f deploy/termrelay/docker-compose.yml config`.
- [x] Re-read this plan and verify there are no unchecked implementation tasks before marking the project complete.
