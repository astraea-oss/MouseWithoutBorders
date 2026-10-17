# Shared Quickshell notifications

The initial implementation synchronizes retained desktop notifications between
explicitly enrolled Linux PCs over their existing encrypted edge-kvm connection.
One configured PC is the authority, independent of input direction. Quickshell
keeps its existing notification centre; edge-kvm adds no notification UI.

## Set up in the existing settings window

Install the updated Linux build on each computer, then open **Settings →
Notifications** from the tray:

1. Turn on **Shared history**. The connected computer is selected automatically;
   older offline pairing records are under **Other saved pairings**.
2. Choose the same computer under **History owner** on both PCs. The connected
   computer is available directly; selecting it also enrolls it for sharing. The initial
   default is deterministic for a pair, so the two computers agree without copying
   fingerprints. Choose the main PC before the first Save if it should own history.
3. Press **Save**. The app installs its bundled bridge into the running Quickshell
   configuration, backs up changed shell files, saves enrollment and restarts
   edge-kvm. Quickshell picks up the configuration change through its normal reload.

The settings page reports bridge readiness, whether a sharing link is connected,
retained notification count, queued operations and errors. **Reload bridge**
refreshes the integration and explicitly requests a Quickshell reload. Installed
files and a connected bridge are reported separately. Symlinked dotfiles retain
their launch path for QML imports and their watched shell inode. If multiple shell
configurations are running or no shell can be detected, choose `shell.qml` in that
same settings page. The installer supports the reviewed Tera notification centre
and the updated Ember laptop centre; an unfamiliar layout is rejected before
modifying shell files or enabling sharing. It never installs a separate notification
centre. DND remains local, and phone notifications remain local in this milestone.

The owner can be corrected before a replica has received any shared history.
Its local queued notifications are preserved if the initial owner is corrected.
After authoritative records or a coordinator epoch are received, ownership is
fixed to protect history. Disabling sharing preserves its identity and history.
The PCs confirm that they selected the same owner before exchanging history;
a transport connection alone is not reported as working notification sharing.
This uses `notification-sync-v2`; both PCs need this version. Older peers retain
normal KVM operation while notification sharing waits for their update.

For an existing service installation, upgrading is:

```bash
cargo build --release -p edge-receiver-linux
./scripts/install-linux-receiver.sh
```

The installer preserves an existing config, device identity and pairings.

## Manual configuration (advanced)

Build and deploy the same updated Linux binary on both computers. Keep their
existing identities and pairings. Copy the fingerprints from their pairing/status
information or startup log; use the full existing colon-separated form.

On the main PC, add to its existing configuration:

```toml
[notifications]
enabled = true
coordinator_fingerprint = "MAIN_PC_FINGERPRINT"
peers = ["LAPTOP_FINGERPRINT"]
```

On the laptop:

```toml
[notifications]
enabled = true
coordinator_fingerprint = "MAIN_PC_FINGERPRINT"
peers = ["MAIN_PC_FINGERPRINT"]
```

These placeholders must be replaced with actual pinned fingerprints. Pairing alone
does not enable sharing. Leave `enabled = false` or omit this section to retain
existing behaviour. Restart edge-kvm after configuration changes. Do not change
coordinator identity as a shortcut for recovery or input-role changes.

## Connect the existing shell

Copy `EdgeNotificationBridge.qml` beside your existing `shell.qml`. Keep the
bridge state at the top level of `ShellRoot`:

```qml
PersistentProperties {
    id: edgeNotificationState
    reloadableId: "edgeNotificationBridgeState"
    property string epoch: ""
    property string offlineClosures: "[]"
}
EdgeNotificationBridge {
    server: notificationServer
    state: edgeNotificationState
    timestamps: root.notificationTimes
    socketPath: "/absolute/path/to/edge-kvm/state/notifications/bridge.sock"
}
```

Alternatively, omit `socketPath` and launch Quickshell with
`EDGE_KVM_NOTIFICATION_SOCKET` pointing to that socket. The backend uses the
existing `EDGE_KVM_STATE_DIR` override when supplied; otherwise state is portable,
next to its executable. It does not discover OS config directories. Unix socket
paths have a platform length limit; use a shorter configured state directory if
binding fails.

The existing `NotificationServer.onNotification` must retain ordinary notifications
before applying DND. Handle restored entries silently, and for fresh retained
entries use the supplied canonical timestamp:

```qml
root.retainNotification(notification,
    Number(notification.hints["x-edge-kvm-posted-ms"]) || Date.now());
```

After tracking the notification, return before creating a popup or sound when
`root.dnd || notification.hints["x-edge-kvm-replay"]` is true. Keep the merged
Tera transient-handling fix: transients are popup-only and automatically expire;
DND and popup timeout must never call `dismiss()`.

The laptop port helper imports Tera's notification-only sections while retaining
its battery, Bluetooth, Wi-Fi and monitor configuration. It refuses to apply twice
and creates a timestamped backup before writing:

```bash
python3 scripts/update-quickshell-notifications.py \
  --reference /path/to/astraea.files/Tera/home/.config/quickshell/shell.qml \
  --target /path/to/laptop/shell.qml \
  --socket /absolute/path/to/state/notifications/bridge.sock
```

This helper targets the older Ember laptop layout reviewed during implementation;
it is not a general installer for arbitrary Quickshell configurations. The reference
used was astraea.files merge commit `ef8ee2c57b20319f8572f3667ae3610d6a20b7da`.

## Behaviour and limits

- Local posts and replacements enter the shared set. A source notification keeps
  its identity across a shell reload.
- Remote copies appear silently in the normal centre. Fresh remote popups and
  remote actions/replies are deferred. Existing local actions continue working.
- Individual dismiss and Clear synchronize removals. Dismissed instances cannot
  be resurrected by stale inventories or updates.
- DND stays local. Popup expiry, shell reload, and remote programmatic close do
  not become user dismissals. Source withdrawal/expiry removes its shared entry.
- With the coordinator offline, local notifications continue, cached shared
  entries remain, and operations queue durably for the next connection. No
  automatic leader election occurs.
- A shell reconnect reconciles its complete inventory before projecting copies.
  Cached entries missing after a cold shell restart are restored silently.
- Notification state and pending operations survive edge-kvm restarts. Quickshell's
  own DND, epoch and offline closure queue survive configuration reloads, not a
  full Quickshell process restart. Closures not yet delivered to edge-kvm can be
  lost if Quickshell itself is killed; do not treat its reload state as durable
  notification storage.
- The first version uses complete staged snapshots rather than incremental event
  replay. Network queues are bounded and state writes run outside the KVM input
  session. Incoming snapshot frames are staged into one worker transaction with
  a shared 32 MiB buffer budget, so a valid snapshot can exceed the 32-message
  worker queue. Pending operations are sent once per connection and retried on
  reconnect; snapshots and ACKs do not resubmit operations already in flight.
  Duplicate operations, ACKs and unchanged snapshots avoid redundant state writes.
  A notification worker failure/overflow disables notification sync for
  that session; reconnect after fixing the reported cause.
- Storage is capped at 4,096 records including terminal markers, 1,024 pending
  operations, and 32 MiB serialized state. At capacity, notification operations
  fail with a diagnostic; native notification delivery continues. Automatic
  tombstone compaction is deferred, so this initial version needs explicit
  maintenance when its record limit is reached.
- The store contains private notification text in plaintext, protected by a 0700
  directory and 0600 files/socket. Removed content is deleted from state while
  minimal terminal markers remain. Bodies are never logged. Icons use bounded
  theme names; remote image paths/URLs are not loaded.
- Legacy phone notifications remain local and are excluded from PC sharing.
  Their desktop ID mappings survive backend restarts in `state/phone-notifications/`,
  with the session bus and notification daemon identity checked before reuse.
  These files contain only IDs and digests. Existing duplicates created before
  this cache was introduced cannot be safely identified from text alone.
  Stable mobile identities, onward-sharing permission, and phone-side dismissal
  require the next mobile protocol milestone.

The initial version has no automatic coordinator migration/reset or remote cache
revocation command. Back up the coordinator store before changes. A replica rejects
an unexpected coordinator store epoch instead of accepting a blank/new store as an
instruction to clear its cache. Do not delete just one device's store to recover:
stop the participants, preserve backups and explicitly reinitialize the set together.
Disabling sync stops propagation but does not erase previously shared information
from other devices.

## Validation

```bash
cargo test -p edge-common -p edge-protocol -p edge-runtime -p edge-receiver-linux
cargo check -p edge-controller-win --target x86_64-pc-windows-msvc
EDGE_KVM_NOTIFICATION_TEST=1 dbus-run-session -- \
  cargo test -p edge-receiver-linux quickshell_bridge_roundtrip -- --ignored --nocapture
```

The last test uses a private D-Bus session and an offscreen Quickshell fixture;
it does not capture the screen or replace the desktop notification service.
It checks real D-Bus ingestion, replacement, remote projection, dismissal, and
reload identity. The ordinary tests cover offline dismissal recovery, stale
updates, terminal removals, enrollment, and coordinator reset detection.

Still perform live acceptance between the physical PCs after both binaries and
shell bridges are installed. Windows notification integration and phone-wide
notification parity are not implemented by this milestone.
