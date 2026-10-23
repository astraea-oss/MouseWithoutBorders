# Shared notifications through Quickshell

Status: Linux PC synchronization, existing-settings setup, source device labels,
and phone notification ingestion are implemented. The laptop notification centre
has been ported from the merged Tera configuration. Final physical two-PC
acceptance, reverse Android dismissal, incremental replay, and automatic
compaction remain outstanding. See
[setup and current limits](../../integrations/quickshell/README.md) for implemented
behaviour; the sections below also include deferred design work.

The sections below describe the target design. The first implementation uses
bounded full snapshots instead of an incremental commit log. Remote entries are
silent; fresh remote popups, remote actions and image transfer are deferred.

## Outcome

Both Linux computers show the same shared notification set in their existing
Quickshell notification centres. Notifications can originate on either computer
or an authorized paired phone. Dismissing an entry on one device clears its
copies everywhere and requests dismissal at its source where supported.

edge-kvm owns synchronization and the canonical shared state. Quickshell remains
the notification server and the only desktop notification UI. No additional
inbox, notification daemon, cloud service, or web UI is introduced.

The initial target is Tera (main PC) and Lua (laptop), both using Quickshell.
The shared model must stay platform independent so future Windows/mobile adapters
can use it without changing conflict rules. Native Windows integration is a
separate milestone, not a prerequisite for these Linux computers.

## Starting point and reference behaviour

This section records the implementation before shared notification support.

- `crates/edge-protocol/src/lib.rs` has `notifications-v1`: phone-to-receiver
  posts, removals, and reconnect snapshots. It has no reverse dismissal command
  or PC-to-PC notification synchronization.
- `apps/edge-receiver-linux/src/notifications.rs` maps each phone's IDs to
  freedesktop notification IDs. It replaces updates and closes removed entries;
  it is currently a view-only mirror, not a shared authoritative store.
- `apps/edge-receiver-linux/src/phone.rs` enforces phone notification permissions
  and keeps mirrors across phone reconnections.
- The [Tera Quickshell configuration at 06d09ed](https://github.com/astraea-oss/astraea.files/blob/06d09edeab62ec0c85eec9d8c2526803dc9ec3be/Tera/home/.config/quickshell/shell.qml)
  supplies the presentation baseline: newest first, app icon/name, timestamp,
  summary/body, individual dismiss, Clear, DND, and independently timed popups.

Quickshell implements the freedesktop Desktop Notifications Specification; this
is the interoperability contract, while the dotfiles define local presentation.
The standard does not provide an API to enumerate a notification centre's history,
so posting via D-Bus alone cannot capture all local entries. A small Quickshell
bridge is necessary.

Completed prerequisite: fix [astraea.files issue #1](https://github.com/astraea-oss/astraea.files/issues/1).
The original Tera shell called `dismiss()` on arrival during DND, losing history
and making suppression indistinguishable from user dismissal. The reviewed
notification lifecycle fixes were ported to Lua while preserving its local layout.

## One authority

Configure Tera's pinned device identity as the notification coordinator. Its
notification hub is the single writer of committed shared state. Lua and phone
gateways submit operations and maintain caches; they do not become authorities
when disconnected. Coordination is independent of KVM controller/receiver roles,
input forwarding, and the connection initiator.

```text
Tera local apps -> Tera Quickshell <-> local bridge <-> Tera notification hub
                                                        ^       |
                                             encrypted operations/state
                                                        |       v
Lua local apps -> Lua Quickshell <-> local bridge <-> Lua cache/outbox
                                                        ^
                                                        |
                                                 paired phone gateway
```

Phones may connect through either authorized computer; the gateway relays source
operations to Tera without changing the phone's origin identity. Do not broadcast
phone content to other trusted peers implicitly: participation in this shared set
requires explicit permission, including onward sharing from each phone.

If Tera is unavailable, local notifications continue working. Lua keeps its last
committed shared view, adds local provisional entries, and queues changes. A local
dismissal hides the entry immediately, but other devices converge only when the
coordinator can be reached. No automatic election or split-brain fallback in v1.
Moving the coordinator requires an explicit stop/export/import operation; it must
never create two live writers for the same set.

## Identity and state

Introduce a small platform-independent notification state module, shared by the
hub and clients. Keep UI objects and D-Bus IDs out of the canonical model.

| Field | Purpose |
| --- | --- |
| Shared key | Origin pinned fingerprint + origin epoch + notification instance ID |
| Source revision | Monotonic origin revision; rejects stale posts/updates |
| Operation ID | Persisted sender epoch/counter; makes retries idempotent |
| Commit cursor | Coordinator store epoch + increasing sequence |
| Content | App label, title/body, posted time, bounded icon, relevant hints |
| Lifecycle | Active or dismissed/removed, with reason and source-dismissal status |
| Adapter mapping | Local server generation/ID or phone key; local-only metadata |

Use opaque strings for 64-bit counters and IDs in JSON sent to QML, avoiding
JavaScript integer precision loss. Network serialization remains typed MessagePack.
Wall-clock time controls display only, never conflict resolution.

An origin epoch changes when its native ID namespace resets; Quickshell reloads
that retain live notifications must preserve their existing shared keys. A newly
posted notification after closure gets a new instance even if a native ID is
reused. An update replaces content under the existing shared key.

The coordinator serializes accepted operations and commits before acknowledging.
Once dismissed, an instance cannot be resurrected by a delayed post, update, or
snapshot. A genuinely new notification must have a new instance key. Repeated
dismissals are harmless. Clear sends the explicit keys currently displayed,
captured before mutating the list; it cannot delete an unseen later arrival.

## Lifecycle contract

| Event | Shared effect |
| --- | --- |
| Source posts/updates | Upsert the same instance at a newer source revision |
| User dismisses / presses Clear | Commit dismissal, remove copies, request source dismissal |
| Popup finishes its animation | None; history remains |
| DND enabled/disabled | Local presentation only; no shared lifecycle operation |
| Quickshell reload / bridge disconnect | None; reconnect and reconcile |
| Authoritative source withdraws/expires | Remove the shared instance with its actual reason |
| A remote copy expires unexpectedly | No global removal; reconcile its local projection silently |

Follow Quickshell's distinction between `dismiss()` and `expire()` and the
freedesktop closure reasons: expired (1), user dismissed (2), programmatic close
(3), and unspecified (4). Only a verified user dismissal becomes a dismissal
request. Track programmatic applies before they can trigger callbacks; do not
infer user intent from an item disappearing from the model.

Transient notifications remain local popup-only events in v1, outside the retained
shared set. Preserve urgency and resident metadata where supported. Local actions
continue working; remote copies must not display nonfunctional action buttons.
Remote action invocation and inline replies are deferred until an authenticated
route back to the originating application exists.

Phone notifications that cannot be dismissed may disappear from the shared inbox
while their native phone original remains. Record the refusal and suppress reposts
of that same instance; do not claim the phone accepted removal. Phone-wide parity
depends on mobile support, permissions, and native notification restrictions.

## Quickshell bridge

Keep `NotificationServer` and the existing notification list in Quickshell. Add a
small reusable bridge component, shared between the Tera and Lua dotfiles:

1. Connect to the local edge-kvm process over a private Unix socket using
   `Quickshell.Io.Socket`. Use versioned, bounded newline-delimited JSON with
   explicit message types, acknowledgements, and reconnect backoff.
2. Observe original local posts, replacements, and closure reasons; send typed
   source operations. Reconnect with a complete local-origin inventory. Verify
   replacement/update observation against the installed Quickshell version in
   the integration spike, not only its new-notification signal.
3. Let the Rust adapter post remote copies using `Notify` and replace existing
   mapped IDs; remove them using `CloseNotification`. Quickshell renders them in
   its normal list. Do not acquire the notifications D-Bus service name.
4. Tag remote copies with vendor hints carrying the shared key, origin, and
   replay status. Register expected copies through the authenticated local bridge
   before posting. Match those records to the actual result IDs and quarantine
   early callbacks so a fast dismissal cannot escape mapping. Unrecognized hints
   alone must not authorize remote lifecycle operations or source impersonation.
5. Suppress forwarding of projection posts/removals back as original events.
   Export user dismissals of projections as shared-key requests. Source-native
   closures and remote-apply closures take distinct paths.
6. During DND, track eligible notifications and skip popups/chimes. After reload
   or reconnect, reconcile silently without replaying sounds or popups. Use the
   source timestamp so synchronization does not reorder old entries as new.

Keep DND, pinning, panel visibility, and popup timers local. Maintain current
compact rows; include source device in the existing app label if needed. Honour
body/icon capability support. Treat received body text as plain text and transfer
bounded image bytes or known icon names, never remote filesystem paths or URLs
to fetch. Advertise only capabilities actually implemented by the shell.

If the bridge is unavailable, local Quickshell still functions and the hub retains
committed state. Resume shared projection after bridge reconciliation; a shell
restart or missing snapshot is never equivalent to Clear.

## Protocol, trust, and reconnects

Add a negotiated `notification-sync-v1` extension and a separate sync event family;
do not change the meaning of existing `notifications-v1` frames. Proposed messages:
`Submit`, `Committed`, `Ack`, `Resume`, `SnapshotStart`, `SnapshotItem`,
`SnapshotEnd`, and source-dismissal request/result. Define precise schemas during
implementation, including limits and protocol errors.

- Send only after capability negotiation and authenticated membership checks.
  Bind origins to authenticated identities; gateway delegation is limited to
  explicitly authorized phones and is validated by the coordinator.
- Extend the phone protocol separately for reverse dismissal and stable instance
  identities. Old phones retain view-only forwarding; do not promise dismissal
  or safe multi-gateway deduplication without those capabilities.
- Store sender operations before sending. Retry until acknowledged. Coordinator
  operation deduplication and source revisions make retries safe.
- Resume from the last applied cursor; otherwise receive a complete staged
  snapshot at cursor N and then commits after N. Install only after SnapshotEnd.
  Overlay pending local dismissals so a snapshot cannot visibly revive them.
- Reconcile queued operations against the authoritative state before exporting
  fresh source inventories. Inventory omission can remove only that source's
  entries, after a complete inventory from the current source epoch. Never remove
  another device's entries because it is absent from a gateway snapshot.
- Do not reuse an abandoned origin epoch after state loss. Retain dismissed-instance
  markers and sender revision watermarks until the epoch is explicitly retired.
  Peers that cannot resume must take a snapshot before sending inventory changes.
- Bound snapshots, queues, content, icons, and per-peer rates. Preserve at least
  the existing phone content limits and the encrypted transport frame limit;
  chunk snapshots rather than sending one growing frame. Coalesce updates to the
  same key without dropping terminal operations. Notification pressure must not
  block input, heartbeat, clipboard, or audio.

Pinning alone does not enroll a device in the shared set. Notification sync is an
explicit per-peer feature. Check membership when accepting operations and when
distributing each entry; enforce phone onward-sharing permission at the hub as
well as the gateway. Unpair/revocation stops distribution and purges that peer's
cached projections when next reachable; remote deletion cannot be guaranteed
while a device is offline or uncooperative.

## Storage and failure handling

Use a bounded transactional store under portable `state/notifications/`, next to
the binary unless the existing directory override is supplied. Store canonical
active entries, cursors, deduplication metadata, and terminal markers on Tera;
clients persist their cache, native mappings, and pending operations. Socket
placement must use the configured private local endpoint directory, with a
portable default; check Unix path-length limits rather than silently relocating
to an OS config directory. Restrict directory/socket/store access to the user and
verify socket peer credentials.

Notification text can contain sensitive information. Persist only what restart
recovery needs, never log bodies, and remove dismissed content/icons after commit,
retaining minimal tombstones. The store is not encrypted at rest by default;
document this when enabling synchronization. Use existing dependencies where
practical; avoid introducing a database service.

Set explicit byte/count limits before implementation ships. At capacity, keep
native local notification delivery working, reject new shared enrollment with a
bounded diagnostic, and require acknowledged compaction or explicit reset. Do not
silently drop pending dismissals or tombstones. Crash recovery must not acknowledge
an operation whose state was not durably saved. Corrupt or missing coordinator
state requires explicit recovery/new store epoch, never treating a blank store
as authority to silently delete all cached entries.

## Delivery sequence

1. **Dotfiles prerequisite and API spike.** Resolve issue #1; extract the bridge
   component; check installed versions, replacement observation, close reasons,
   custom hints, icon rendering, silent replay, and reload identity preservation.
   Update only Lua's relevant configuration after reviewing its local differences.
2. **Shared state and protocol.** Implement the reducer, persistence, identities,
   extension negotiation, membership checks, commit ordering, and reconnect rules
   in reusable Rust code. Add a single process-level hub, not one store per session.
3. **Two Linux computers.** Wire the hub into both PC session directions in
   `apps/edge-receiver-linux/src/main.rs`; extend the existing notifications adapter
   and add the local bridge endpoint. Deliver both local-source capture and remote
   projection, individual dismiss and Clear, offline queueing, and restart recovery.
4. **Phones.** Replace isolated phone mirrors with hub ingestion in `phone.rs`.
   Implement stable mobile keys, source-dismissal results, pending dismissal on
   reconnect, and explicit onward-sharing permissions in the mobile app. Its source
   is outside this repository; this milestone needs coordinated mobile changes.
5. **Rollout.** Document coordinator/peer setup, storage limits, reset/export,
   compatibility and permission behaviour. Keep legacy view-only notification
   forwarding working for peers without sync support; never run both projection
   paths for the same phone notification. Validate on both physical devices.

## Focused validation and acceptance

Automate the failure-prone state transitions: stale updates after dismissal,
duplicate submissions, update/dismiss races, native ID reuse, Clear versus a new
arrival, interrupted snapshots, replay after crash, queue capacity, and unauthorized
origins/phone relays. Use fake adapters for callback ordering and echo prevention.
Run affected Rust tests plus workspace checks when code is implemented; this
document alone does not require runtime tests.

Manual acceptance on Tera and Lua:

- A local notification from either computer appears once in both existing centres.
- An update replaces both entries in place; icons, source time, and Unicode survive.
- Individual dismiss and Clear from either computer remove matching shared entries.
- Popup expiry, DND, panel opening, and Quickshell reload never clear another device.
- DND retains entries silently; disabling it does not replay a backlog of popups.
- Disconnected dismissals survive process restart and converge without resurrection.
- Coordinator downtime preserves local delivery and makes no second authority.
- Source withdrawal removes copies; remote projection expiry cannot delete originals.
- Phone dismissal works when supported and reports refusal honestly otherwise.
- Phone data does not reach a computer without explicit onward-sharing permission.
- Notifications remain independent of KVM role handover and input forwarding state.
- No extra notification UI, polling loop, or OS config-directory writes are added.

## Standards and references

- [Quickshell NotificationServer](https://quickshell.org/docs/v0.3.2/types/Quickshell.Services.Notifications/NotificationServer/)
- [Quickshell Notification lifecycle and metadata](https://quickshell.org/docs/v0.3.2/types/Quickshell.Services.Notifications/Notification/)
- [Quickshell local socket API](https://quickshell.org/docs/v0.3.2/types/Quickshell.Io/Socket/)
- [freedesktop notification D-Bus protocol](https://specifications.freedesktop.org/notification/latest/protocol.html)
- [DND prerequisite issue](https://github.com/astraea-oss/astraea.files/issues/1)

These API references were checked while writing the plan. Confirm compatibility
with both installed Quickshell versions in milestone 1; the dotfiles snapshot is
a behavioural baseline, not evidence that either machine runs a particular API version.


### Settings integration

The existing compact settings window now owns enrollment, history owner selection,
Quickshell bridge installation/update and live health. Saving an enabled setup
installs the bundled no-UI bridge with shell backups before saving the config;
existing settings restart handling applies the change. Paired computer names
replace manual fingerprints. Existing-store ownership changes are refused.
Reviewed Tera and updated Ember shells are supported; unknown layouts are left
untouched and reported in the settings window. A two-PC physical acceptance run
still follows deployment of the updated build on both devices.


### Setup reconciliation fixes

Notification sync v2 begins with history-owner agreement; a mismatched pair fails
with an actionable status before applying history. Connected names come from the
authenticated PC Hello rather than the legacy configured `Peer` label. An uninitialized replica can correct its owner without a manual reset; its
local outbox is retained and its previous store is backed up. Authoritative
records, an accepted epoch or nonlocal operations keep ownership locked. Bridge installation preserves symlink import paths and
the watched shell inode, and reinstalling requests a reload. The integration test
hot-installs into a running symlinked shell with retained history.
