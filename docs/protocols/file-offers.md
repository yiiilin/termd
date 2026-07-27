# File Offer Protocol

Status: accepted on 2026-07-27.

## Purpose

A File Offer lets a local producer ask one termd daemon to notify every paired client that a completed file is available for download. It is daemon-global, transient, and independent of terminal sessions. Creating an offer, observing it, and downloading it use three separate authority surfaces.

## Lifecycle

- One successful creation produces one notification broadcast.
- Notifications are not added to workspace metadata snapshots and are not replayed after reconnect.
- An offer remains resolvable for 24 hours, until daemon restart, or until evicted from the 64-entry in-memory store, whichever comes first.
- The store evicts the oldest active offer when it reaches its limit.
- Offer creation reads the source to record a SHA-256 content digest but does not retain a file copy in daemon storage. A download creates an unlinked private temporary snapshot, requires its digest to match the offer, and validates the original again before returning `200`; removal, replacement, or modification invalidates the offer.
- Download and dismissal are client-local actions and do not consume another client's prompt.
- Re-notification always requires another explicit `termd offer-file` invocation.

## Local Daemon Control

The Daemon Control Protocol is HTTP/1.1 with JSON bodies over a Unix socket. It is enabled by default at `/run/termd/termd.sock` with mode `0600` and may be changed with `--control-socket <PATH>` or disabled explicitly with `--no-control-socket`.

`termd offer-file` selects a socket in this order:

1. `--socket <PATH>`
2. `TERMD_SOCKET`
3. `/run/termd/termd.sock`

The client never scans for sockets. Successful socket access is sufficient authority; the local router requires no application credential or peer-UID match. The daemon resolves and reads the path with its own filesystem authority.

### Create An Offer

```http
POST /v1/file-offers HTTP/1.1
Content-Type: application/json

{"path":"./report.zip"}
```

The path must resolve to a regular file. A symlink to a regular file is accepted; a directory, dangling symlink, FIFO, Unix socket, block device, or character device is rejected.

Success returns `201 Created`:

```json
{
  "offer_id": "d6f0b62a-c890-4b96-b18f-a6696fc3a62b",
  "name": "report.zip",
  "path": "/home/user/project/report.zip",
  "size_bytes": 1234,
  "created_at_ms": 1785144000000,
  "expires_at_ms": 1785230400000
}
```

Acceptance means the daemon read the complete file to establish its content digest, validated and registered the offer, and queued notification work for every currently connected client. Hashing runs outside the serialized protocol state, so a large offer does not block terminal input or metadata traffic. Acceptance does not acknowledge browser receipt or Push-provider delivery. If a connected client has filled its 64-event queue, creation fails before registration with retryable `file_offer_delivery_busy`; the caller may retry after that client catches up or disconnects. `termd offer-file --json` prints the object unchanged; plain output reports acceptance without claiming delivery.

All local errors use the standard application envelope:

```json
{"error":{"code":"file_not_regular","message":"offered path is not a regular file","retryable":false}}
```

Expected error codes include `invalid_request`, `file_not_found`, `file_not_regular`, `file_unreadable`, `file_offer_delivery_busy`, and `control_socket_unavailable`.

### Socket Ownership

A daemon-lifetime advisory lock serializes ownership of the socket path. An existing entry is removed only when all of these are true:

- every ancestor passes no-follow ownership and writability validation, and the direct parent is daemon-owned without group/world write access;
- the entry is a Unix socket rather than a symlink or another file type;
- a connection attempt proves that no listener exists;
- the device and inode still match the inspected entry immediately before removal.

A new socket is first bound inside a daemon-owned `0700` staging directory. After the staged socket is set and verified as `0600`, it is published to the final path with a no-replace rename, so the public pathname never exposes the bind-time mode. A live listener, timeout, ambiguous error, changed inode, symlink, non-socket entry, or publication collision fails daemon startup. Shutdown removes only the socket inode created by that daemon.

## Workspace Event

Every authenticated metadata WebSocket connected when an offer is accepted receives one event:

```json
{
  "type": "file.offer",
  "payload": {
    "offer_id": "d6f0b62a-c890-4b96-b18f-a6696fc3a62b",
    "name": "report.zip",
    "path": "/home/user/project/report.zip",
    "size_bytes": 1234,
    "created_at_ms": 1785144000000,
    "expires_at_ms": 1785230400000
  }
}
```

`file.offer` has no metadata revision and never appears in `metadata.snapshot` or `metadata.update`. A client retains the resulting Offer Prompt only in its current page lifecycle.

## Web Push

Every active Push subscription is eligible for File Offer delivery. The encrypted payload contains only routing identifiers:

```json
{
  "version": 1,
  "kind": "file_offer",
  "server_id": "570643d9-2c79-4de2-942d-bcb100f2463f",
  "offer_id": "d6f0b62a-c890-4b96-b18f-a6696fc3a62b"
}
```

The system notification uses generic localized text and contains no file name, path, size, or download authority. Selecting it opens or focuses the matching daemon and carries `server_id` and `offer_id` to the application. The application resolves the offer after authentication and presents the in-app prompt; system-notification selection does not start a download.

termd has no application-level notification mode. In-app delivery is always enabled. System delivery requires the browser or operating system permission, requested through a one-time user action in the first paired workspace; denial degrades to in-app delivery.

## Browser HTTP Interface

All browser JSON routes require the normal v0.7 Bearer access token and standard error envelope. No route lists active or historical offers.

### Resolve An Offer

```http
GET /api/files/offers/{offer_id}
Authorization: Bearer <access_token>
```

Success returns the same File Offer object as creation. Unknown offers return `file_offer_not_found`; expired, evicted, deleted, replaced, or changed offers return a non-retryable invalid/expired response. This route lets a Push click restore one specific prompt without replaying workspace notifications.

### Prepare A Download

```http
POST /api/files/offers/{offer_id}/downloads
Authorization: Bearer <access_token>
Content-Type: application/json

{}
```

The daemon reopens and validates the original regular file, authenticates the requesting paired device, creates a grant with a maximum lifetime of 60 seconds, and returns `201 Created`:

```json
{
  "download_id": "0529b4f6-5f59-4a28-bc29-b323cfdbb0ed",
  "download_url": "/api/files/offer-downloads/0529b4f6-5f59-4a28-bc29-b323cfdbb0ed?server_id=570643d9-2c79-4de2-942d-bcb100f2463f",
  "name": "report.zip",
  "size_bytes": 1234,
  "expires_at_ms": 1785144060000
}
```

The response also sets a random grant secret in an `HttpOnly`, `SameSite=Strict` cookie. The grant records the authenticated device, offer, and download ID; the cookie is scoped as narrowly as the browser-visible route permits, marked `Secure` on HTTPS, expires with the grant, and is cleared after consumption. The cookie is a bearer capability after issuance: copying the complete cookie transfers its remaining one-time authority. `HttpOnly`, same-site isolation, short expiry, path scoping, and single consumption are the protection boundary; the download ID alone is not authorization. The optional `server_id` query parameter is only a relay routing hint and grants no access; direct daemon listeners ignore it.

Credentialed File Offer responses mirror the requesting `Origin` and send `Access-Control-Allow-Credentials: true`. This supports the normal same-origin deployment and same-site development origins such as two ports on the same host. A cross-site UI cannot rely on a `SameSite=Strict` or third-party cookie and is outside the native-download contract; serve the UI from the daemon or relay origin instead.

### Stream A Download

```http
GET /api/files/offer-downloads/{download_id}
Cookie: <matching one-time grant cookie>
```

The daemon validates and consumes the matching grant, copies the reopened source into a private unlinked temporary file while calculating its SHA-256 digest, requires that digest to match the version recorded at offer creation, and validates the source identity again after the copy. Only then does it return `200` and asynchronously stream the stable snapshot with `Content-Length`, `Cache-Control: private, no-store`, and a safe basename in `Content-Disposition`. Reuse, expiry, the wrong cookie, a changed source, a digest mismatch, or an incomplete snapshot fails before a successful response. `HEAD` is rejected without consuming the grant.

The response has no artificial size limit and does not require JavaScript to buffer the body, allowing native download handling on iPhone and desktop browsers. Snapshot preparation can require temporary space equal to the source file and delays response headers until the snapshot is complete; the temporary file is removed automatically when the response ends.

## Relay And Compatibility

- Relay admission adds exact allowlist entries only for the three browser routes above. `/v1/file-offers` remains local and can never enter a relay tunnel.
- Relay stores no offer, file, cookie, grant, notification, or delivery state and does not interpret `file.offer`.
- This is an additive v0.7 extension. Older clients ignore the unknown event; no supervisor compatibility change or session rebuild is required.
- For relay access, deploy the relay allowlist before clients depend on the new download routes.

## Agent Skill

The embedded skill is named `termd-file-offer` and is installed only by explicit command. Default personal locations are:

```text
Codex:    ${CODEX_HOME:-$HOME/.codex}/skills/termd-file-offer
Claude:   $HOME/.claude/skills/termd-file-offer
OpenCode: ${XDG_CONFIG_HOME:-$HOME/.config}/opencode/skills/termd-file-offer
```

`termd skill install --agent auto` installs the current bundled skills for detected agents. Matching content is an idempotent success; modified content is preserved unless `--force` is supplied. Uninstall removes only content that still matches a bundled version.
