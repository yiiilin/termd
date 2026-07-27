# Daemon File Offers

## Goal

Let an AI agent explicitly offer a completed local file through the `termd` binary so every paired browser can act on a download prompt without finding the file in the session file browser. A File Offer is daemon-global and transient; it is not owned by a terminal session or stored as notification history.

## Confirmed User Contract

- `termd offer-file <PATH> [--socket <PATH>] [--json]` offers exactly one file per invocation.
- The command has no custom message or display-label option; clients derive the title from the file name and show the canonical path separately.
- Command success means the daemon validated and registered the offer and queued notification work; it does not claim that any client or Push provider received it. JSON output contains the offer ID, canonical path, size, and expiry.
- The command accepts a regular file or a symlink whose resolved target is regular. Directories, FIFOs, sockets, block devices, and character devices are rejected; AI agents package directories before offering them.
- Each invocation broadcasts once. Reconnect does not replay the notification, and notifying again requires another invocation.
- An active offer remains resolvable for at most 24 hours, is cleared by daemon restart, and is evicted oldest-first after 64 active offers.
- Offer creation hashes and references the original file without retaining a persistent copy. Native download uses a private unlinked temporary snapshot, requires its SHA-256 digest to match the offered version, and revalidates the original after copying; removal, replacement, or modification invalidates the offer and requires a new invocation.
- Authenticated in-app prompts show the file name, canonical absolute path, and size. System notifications contain no file name, path, size, or download authority.
- Starting a native download returns the foreground prompt to its ready state so the user can retry or dismiss it; the prompt otherwise remains until explicit dismissal or the current page lifecycle ends. One client's action does not affect another client.
- Desktop shows a compact prompt at the upper right; mobile shows it below the title bar. Selecting the prompt downloads; its close control only dismisses that client-local prompt.
- Multiple prompts remain independent and are ordered newest first.

## Daemon Control Interface

- A new daemon-wide Unix socket is enabled by default at `/run/termd/termd.sock` for both managed and manually started daemons; it is distinct from per-session supervisor sockets.
- Socket mode remains `0600`. Successful socket access is the entire caller authorization; no peer UID policy or application credential is added.
- An authorized caller may ask the daemon to offer any regular file readable by the daemon, even if the caller could not read it directly.
- The extensible Daemon Control Protocol uses versioned HTTP/1.1 JSON routes on a dedicated local router. Its first command is File Offer creation; the router is never mounted on the network listener or exposed through relay admission.
- Client socket selection is `--socket`, then `TERMD_SOCKET`, then `/run/termd/termd.sock`. A custom daemon listener uses `--control-socket`; neither side scans for sockets.
- A missing parent, unsafe path, permission error, or live path collision fails daemon startup. The daemon never silently disables, relocates, or takes over the control socket.
- `--no-control-socket` explicitly disables the default listener for restricted or compatibility environments.
- A daemon-lifetime lock guards socket ownership. Only a verified Unix socket with no reachable listener and an unchanged inode is removed as stale; live, ambiguous, symlink, and non-socket paths fail startup. Shutdown removes only the daemon's own inode.
- Managed installation creates the runtime directory without changing supervisor compatibility or rebuilding existing sessions.

## Notification Model

- Every daemon notification event is enabled; termd removes the application-level `off`, `mentions`, and `all` preferences.
- In-app notification delivery needs no platform permission.
- The first paired workspace presents a one-time user action that invokes the browser's native notification permission prompt.
- Web Push is best effort and only available after browser and operating-system permission. Permission denial leaves in-app delivery intact.
- Foreground clients suppress duplicate system notifications.
- Selecting a system notification focuses or opens termd and restores the matching client-local prompt. The user then selects that prompt to download, which preserves iPhone PWA user-activation requirements.

## Agent Skill

- The `termd` binary embeds a concise file-offer skill and installs it only on explicit request.
- The bundled skill is named `termd-file-offer`; future daemon capabilities use separate focused skills rather than expanding one always-loaded general skill.
- `termd skill install --agent auto` installs for detected supported agents under the invoking user's configuration.
- `termd skill status`, `termd skill print`, and `termd skill uninstall --agent <name>` provide inspection and removal.
- The skill triggers after an agent completes a file intended for user download, packages directories itself, invokes `termd offer-file`, and reports success only after the command succeeds.
- Install and upgrade never mutate an agent's configuration automatically.
- Installation is idempotent when contents match. A modified target is preserved unless the user passes `--force`, and uninstall refuses to delete modified content.

## Security Invariants

- Creating an offer is possible only through the local Daemon Control Socket.
- Network HTTP, workspace WebSockets, terminal output, escape sequences, and relay routes cannot create an offer.
- The relay remains a trusted admission and transparent routing layer and stores no offer or file state.
- File paths and download grants never enter Web Push payloads or URLs.
- A download grant is high-entropy, short-lived, single-use, and issued only after authenticating the requesting paired device. Its `HttpOnly` cookie is a bearer capability and is not claimed to resist complete cookie copying.
- File type and identity are checked after opening; a changed original cannot silently become the content of an older offer.
- File Offers have no artificial size limit. Browsers use a native attachment response authorized by a one-time `HttpOnly`, `SameSite=Strict` cookie plus a non-authorizing download ID, so JavaScript never buffers the file.
- Unsupported native download environments fail explicitly and never buffer an unbounded file in JavaScript memory.

## Impact

- `termd`: CLI parsing, local HTTP-over-UDS listener, bounded File Offer state, metadata event delivery, download authorization, Push coordination, and managed installation.
- `proto`: typed File Offer payloads and exact relay tunnel admission only for browser-side download operations; local creation routes remain absent.
- `termui/frontend`: ephemeral global prompt state, direct download action, notification permission onboarding, settings removal, and Service Worker routing by offer ID.
- `termrelay`: transparent forwarding tests only; no business state or offer interpretation.
- Agent integrations: embedded skill templates and explicit per-agent installation adapters.

## Verification

- CLI and local socket tests cover default/custom selection, unavailable daemon, HTTP framing, malformed input, permissions, and daemon-readable paths.
- File tests cover regular files, symlinks, directories, special files, deletion, replacement, modification, 24-hour expiry, eviction, and daemon restart.
- Multi-client tests prove one broadcast reaches every connected client, is not replayed, and one client's download or dismissal does not consume another client's offer.
- Authorization tests prove local creation routes are unreachable over network and relay, while download grants reject the wrong device, expiry, and reuse.
- Frontend and Service Worker tests cover prompt lifecycle, multiple offers, generic Push content, permission denial, notification click restoration, direct and relay downloads, and the iPhone native large-file path.
- Full Rust and frontend checks run after focused tests; browser screenshots verify desktop and mobile placement without covering the fixed title or terminal controls.

## Protocol Contract

- Local creation is `POST /v1/file-offers` over the Daemon Control Socket and returns `201 Created` with the File Offer object.
- The metadata socket emits one unrevisioned `file.offer` event with the same object; it is absent from snapshots and updates.
- Web Push carries only `version`, `kind`, `server_id`, and `offer_id`.
- `GET /api/files/offers/{offer_id}` resolves one known offer after a Push click; no list route exists.
- `POST /api/files/offers/{offer_id}/downloads` authenticates the paired device and creates a short-lived bearer-cookie grant.
- `GET /api/files/offer-downloads/{download_id}` requires the matching cookie and streams a native attachment.
- The complete wire contract is maintained in [File Offer Protocol](../protocols/file-offers.md).

## Execution Tasks

- [x] Add the bounded daemon-global File Offer module with file identity validation, expiry, eviction, and focused tests.
- [x] Add safe default/control/disabled Unix-socket lifecycle plus HTTP-over-UDS creation and CLI tests.
- [x] Add the unrevisioned metadata event and best-effort Web Push event without snapshot replay.
- [x] Add authenticated offer resolution, cookie-bound download preparation, native streaming, and exact proto/relay admission tests.
- [x] Remove application notification modes and add first-workspace browser permission onboarding with migration tests.
- [x] Add desktop/mobile Offer Prompt state and direct download behavior, including multiple prompts and failure retention.
- [x] Embed `termd-file-offer`, add Codex/Claude/OpenCode installation adapters, and verify conflict-safe install/status/print/uninstall behavior.
- [x] Update managed installation for `/run/termd/termd.sock` without changing supervisor compatibility or session state.
- [x] Run focused and full Rust/frontend verification, direct/relay browser QA, desktop/mobile screenshots, and large-file iPhone-path tests. Automated iPhone coverage uses Linux Playwright WebKit with an iPhone 13 profile; real iPhone Chrome PWA hardware remains a manual acceptance check.
- [x] Review the final security-sensitive diff and confirm no task-created temporary artifacts remain.

## Rollback And Stop Conditions

- The feature is additive to session and supervisor state; implementation must not change supervisor compatibility or rebuild, close, or clear existing sessions.
- Removing the local listener, offer event, and frontend prompt restores prior behavior without a persisted-state migration.
- Stop and redesign if implementation requires relay-owned offer state, a session-scoped offer, credentials in URLs, or a daemon restart that cannot preserve supervisors.
- Stop and reassess large-file delivery if iPhone support would require buffering the complete file in browser memory.
