# termd Domain Language

termd provides persistent terminals through one trusted daemon and its paired clients. This glossary names the domain concepts that must remain consistent across the daemon, relay, clients, and command-line interface.

## File Delivery

**File Offer**:
A transient, daemon-wide invitation for paired clients to download one completed regular file. A symlink may name the file, but its resolved target must be regular; directories and special files are not accepted. A File Offer does not belong to a terminal session and is not a durable inbox item.
_Avoid_: Session file offer, file message, artifact record

**Offer Notification**:
The single attention hint emitted when a File Offer is created. It is not replayed automatically; another hint requires a new explicit offer.
_Avoid_: Reminder, inbox entry

**Offer Acceptance**:
The daemon's confirmation that it validated and registered a File Offer and queued its notification work. It is not proof that any particular client or Push provider received the notification.
_Avoid_: Delivery receipt, client acknowledgement

**Offer Prompt**:
The client-local, actionable presentation of an Offer Notification. Starting a native download leaves it available for retry; it remains until explicit dismissal or the current page lifecycle ends, but is not retained as notification history.
_Avoid_: Toast, inbox item, offer history

**Offer Path**:
The canonical absolute path of the offered file, visible inside authenticated termd clients but excluded from system notifications.
_Avoid_: Input path, relative path, download URL

**Offered File Version**:
The original file version referenced when a File Offer is created. The daemon records its SHA-256 digest but does not retain a persistent copy. Native download makes an unlinked temporary snapshot, requires the digest to match, and validates the source again before returning `200`; the offer becomes invalid when the original file is removed, replaced, or changed.
_Avoid_: File snapshot, latest file, cached artifact

**Download Grant**:
A client-specific permission to perform one download from a File Offer. It is distinct from the File Offer itself.
_Avoid_: Download link, file URL

**Offer Producer**:
A local process permitted to create File Offers by its access to the daemon's local offer channel, independent of its operating-system user identity. It may offer any regular file that the daemon can read.
_Avoid_: Session owner, daemon user, Agent

**Daemon Control Socket**:
The daemon-wide local Unix socket used for privileged local capabilities such as creating a File Offer. It is distinct from every session's internal supervisor socket.
_Avoid_: Session socket, terminal socket

**Daemon Control Protocol**:
The versioned, extensible set of explicit local commands accepted through the Daemon Control Socket. It is not a general-purpose transport for terminal, HTTP, or relay traffic.
_Avoid_: Local HTTP, terminal protocol, arbitrary RPC tunnel

## Notifications

**In-App Notification**:
An actionable notice presented inside an active paired client. It is always enabled by termd and does not depend on operating-system notification permission.
_Avoid_: Push notification, optional alert

**System Notification**:
A browser- or operating-system-level notice delivered when termd is not visible. It is eligible by default but can only be delivered after the user grants the platform notification permission.
_Avoid_: In-app prompt, notification preference
