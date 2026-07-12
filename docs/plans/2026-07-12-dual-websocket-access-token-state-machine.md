# Dual WebSocket and Access Token Protocol Plan

Date: 2026-07-12
Target release: 0.7.0
Supervisor compatibility: `2026-07-12-dual-ws`

## Goal

Replace the Web UI's event-triggered HTTP polling and session-scoped terminal
transport with two stable workspace WebSockets, a JSON-only HTTP control API,
and a termd-signed device/access credential chain that can be verified by the
trusted relay without synchronized token state.

## Scope

### Workspace connections

Each Web UI workspace maintains exactly two WebSocket connections:

1. `/ws/metadata` publishes a revisioned initial snapshot and subsequent
   session, client, daemon status, RTT, and CWD updates.
2. `/ws/terminal` owns one current session. Its first command is either
   `terminal.create` or `terminal.attach`; the daemon replies with session
   information when creating, then a terminal snapshot, then PTY stream data.

The terminal snapshot includes a 1-based `cursor: { row, col }`. After the
snapshot, cursor movement is derived only from PTY output. Input carries bytes,
not cursor coordinates. Resize is a terminal WebSocket command.

Closing the current session disables input, closes the terminal WebSocket
without waiting, and calls only the JSON `session.close` HTTP route. It must not
trigger attach, list, or clients requests. Metadata is pushed on its dedicated
WebSocket rather than fetched in response to session events.

### HTTP API

Every application HTTP response is JSON, including errors, 404, and 405.
Raw upload chunk and download byte bodies are the only exemptions. Errors use:

```json
{"error":{"code":"...","message":"...","retryable":false}}
```

Authentication routes:

- `POST /api/auth/pair`
- `POST /api/auth/challenge`
- `POST /api/auth/access-token`
- `POST /api/auth/device-certificate/migrate`

Authorization schemes are `TermdPair <ticket>`, `TermdDevice <certificate>`,
and `Bearer <access_token>`.

Typed HTTP control routes remain for session reorder, rename, close, legacy
control noop, files, search, git, git diff/action, file read/write/delete, and
daemon client forget. Session list/attach/cursor/resize, daemon clients/status,
legacy download chunk/prepare, session scope tokens, HTTP `ProtocolPacket`
envelopes, runtime/HTTP E2EE, and fixed files polling are removed.

File transfer routes are:

- `POST /api/files/uploads`
- `PUT /api/files/uploads/{id}/chunks`
- `POST /api/files/uploads/{id}/commit`
- `POST /api/files/uploads/{id}/abort`
- `POST /api/files/downloads`
- `GET /api/files/downloads/{id}`

### Credential chain

The daemon is the identity anchor and signs:

- an expiring Pair Ticket;
- a persistent Device Certificate, usable only with proof of the matching
  device private key;
- a five-minute Access Token.

Signatures use fixed-algorithm Ed25519 compact JWS with `kid=server_id`. The
relay stores only `server_id -> daemon_public_key` and verifies credentials
offline; it does not synchronize tickets, certificates, tokens, or PTY state.
Existing paired devices obtain certificates through the restricted migration
endpoint. Access-token refresh begins 60 seconds before expiry. Both WebSocket
routes authenticate at the route boundary and reconnect with a refreshed token.

There is no operator capability layer and no E2EE layer. Every paired and
attached device is an operator under the existing shared-control model. The
relay is trusted admission and routing infrastructure.

## State Machines

```text
Device       UNPAIRED -> PAIRED -> REVOKED
Access token NONE -> CHALLENGED -> ACTIVE -> REFRESHING -> ACTIVE | EXPIRED
Connection   INIT -> AUTH -> ATTACHED -> CLOSED
Metadata     EMPTY -> SYNCED -> STALE -> RESYNCING -> SYNCED
Terminal WS  CLOSED -> CONNECTING -> OPENING -> SYNCING -> STREAMING
             -> CLOSING -> CLOSED
Renderer     EMPTY -> RESETTING -> READY -> APPLYING -> DESYNC -> RESETTING
Session      CREATED -> RUNNING -> CLOSED
Supervisor   RUNNING -> CLOSING -> CLOSED
```

Metadata events carry a monotonically increasing revision. A revision gap marks
the client stale and causes a metadata WebSocket resync. A terminal snapshot
resets the renderer before stream application; stream desynchronization causes
terminal reconnection and another snapshot rather than an HTTP cursor fetch.

## Invariants

- Only an authenticated, attached terminal connection can read or write a PTY.
- An unpaired device cannot mint an access token or open workspace WebSockets.
- Device certificates require device-key challenge proof and can be revoked.
- The relay verifies admission but never owns session, PTY, or control state.
- A client disconnect never terminates a persistent session.
- A workspace has one metadata WebSocket and one terminal WebSocket, with no
  event-triggered list/clients/cursor/resize HTTP traffic.
- Secrets and bearer credentials never appear in URLs or logs.

## Compatibility and Rollout

This is a public protocol break released as application version 0.7.0. Update
README installation/upgrade instructions, protocol documentation, fixtures,
and release notes together. Upgrade relay first, preserving OpenResty routing to
`127.0.0.1:18765`, then upgrade local termd. Supervisor protocol compatibility
changes to `2026-07-12-dual-ws`; the explicitly approved local rollout clears
existing sessions before starting the new daemon. No production session is
cleared during development or test verification.

## Verification and Stop Conditions

Use test-first changes: demonstrate each new contract failing before production
implementation, then make the smallest implementation pass. Run one final
targeted suite covering signed credentials, JSON errors, both WebSockets,
close semantics, metadata revisions, terminal cursor snapshots, and migration.
Run `scripts/qa.sh` exactly once after the final diff is ready. If it hangs,
record the concrete test, PID, and log evidence rather than rerunning it.

An independent read-only reviewer checks the complete final diff against this
plan and test evidence. At most two focused implementation/review rounds are
allowed. Historical failures and low-risk improvements are non-blocking.

After commit and rollout, run direct and relay authentication smoke checks and
20 normal-Chromium create/close cycles through the relay. Acceptance thresholds:

- p50 <= 750 ms
- p95 <= 1.5 s
- max <= 2.5 s
- stable workspace connection count: exactly two

Close only test sessions and confirm no test `__session-supervisor` process or
temporary state remains. Completion requires passing scoped verification, no
diff-caused process leak, no independent-review blocking issue, a final commit,
and successful relay-first then local-daemon rollout.
