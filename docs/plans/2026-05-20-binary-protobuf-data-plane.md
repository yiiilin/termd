# Binary Protobuf Data Plane Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a binary E2EE data plane for termd so encrypted WebSocket business traffic and relay mux traffic no longer wrap terminal bytes in base64 JSON.

**Architecture:** Keep the existing low-frequency JSON route and E2EE handshake for compatibility, then negotiate packet mode and send post-auth business traffic as binary frames. The binary frame format uses a small stable header for encrypted outer frames and relay mux routing; decrypted business payloads use Protobuf-compatible wire encoding with `bytes` fields for terminal input/output.

**Tech Stack:** Rust, TypeScript, ChaCha20-Poly1305 E2EE, WebSocket binary frames, Protobuf wire encoding, existing termd packet stream model.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Task 1: Add Binary E2EE Frame Encoding

**Files:**
- Modify: `termd/src/net/mod.rs`
- Modify: `termui/frontend/src/protocol/e2ee.ts`
- Test: `termd/src/net/mod.rs`
- Test: `termui/frontend/src/__tests__/direct-client.test.ts`

- [x] Write failing Rust tests for binary encrypted frame roundtrip, sequence validation, and non-JSON ciphertext transport.
- [x] Write failing frontend tests for binary encrypted frame roundtrip and old JSON encrypted frame compatibility.
- [x] Implement `termd` binary outer frame encode/decode helpers with magic/version/sequence/ciphertext bytes.
- [x] Implement frontend binary outer frame encode/decode helpers with the same wire format.
- [x] Run focused Rust and frontend tests and verify the new tests pass.

## Task 2: Add Protobuf Packet Wire Encoding

**Files:**
- Modify: `proto/src/lib.rs`
- Modify: `termd/src/net/protocol.rs`
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Create: `termui/frontend/src/protocol/binary-packet.ts`
- Test: `proto/src/lib.rs`
- Test: `termd/src/net/protocol.rs`
- Test: `termui/frontend/src/__tests__/direct-client.test.ts`

- [x] Write failing Rust tests for Protobuf packet request/response/stream_chunk/flow encoding with raw terminal bytes.
- [x] Write failing frontend tests for the same packet shapes and bytes fields.
- [x] Implement Protobuf-compatible packet encode/decode in Rust without changing the existing JSON packet path.
- [x] Implement matching TypeScript packet encode/decode helpers.
- [x] Convert binary packet terminal payloads to raw `bytes` while preserving JSON packet fallback with `data_base64`.
- [x] Run focused protocol and direct-client tests and verify the new tests pass.

## Task 3: Wire Binary Mode Into Direct Daemon WebSocket

**Files:**
- Modify: `termd/src/net/server.rs`
- Modify: `termd/src/net/protocol.rs`
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Test: `termd/src/net/server.rs`
- Test: `termui/frontend/src/test/mock-daemon.ts`
- Test: `termui/frontend/src/__tests__/app.test.tsx`

- [x] Write failing tests proving authenticated packet traffic is emitted as WebSocket binary frames in binary mode.
- [x] Write failing tests proving terminal input/output no longer contains `data_base64` in binary mode.
- [x] Add capability negotiation that keeps JSON handshake but switches post-auth packet traffic to binary when both sides support it.
- [x] Send and receive binary encrypted packets in `termd` server while retaining old JSON encrypted frame support.
- [x] Send and receive binary encrypted packets in frontend `DirectClient` while retaining old JSON encrypted frame support.
- [x] Update mock daemon to exercise binary mode.
- [x] Run focused server/frontend tests and verify direct mode passes.

## Task 4: Convert Relay Mux To Binary Opaque Frames

**Files:**
- Modify: `proto/src/lib.rs`
- Modify: `termrelay/src/ws.rs`
- Modify: `termd/src/net/relay.rs`
- Test: `termrelay/src/ws.rs`
- Test: `termd/src/net/relay.rs`
- Test: `termrelay/tests/relay_e2e.rs`

- [x] Write failing relay tests proving binary client frames stay binary through relay mux without JSON `data_base64`.
- [x] Implement relay mux binary header for client connected/disconnected and opaque frame routing.
- [x] Keep relay dumb pipe: parse only mux routing fields, never decrypted business packet content.
- [x] Update daemon relay connector to send/receive binary mux frames and retain JSON mux compatibility.
- [x] Run focused relay unit and e2e tests and verify relay mode passes.

## Task 5: End-To-End Verification And Cleanup

**Files:**
- Modify: tests as needed
- Modify: docs or changelog only if required by changed behavior

- [x] Run `cargo fmt --check`.
- [x] Run `git diff --check`.
- [x] Run `cargo test --workspace`.
- [x] Run `cd termui/frontend && npm test -- --run`.
- [x] Run `cd termui/frontend && npm run build`.
- [x] Inspect the diff for relay business-logic violations, accidental plaintext logging, and fallback compatibility.
