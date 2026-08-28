# Universal Controller and Receiver Roles

## Implementation status

The protocol/runtime, Linux capture and controller path, v2 migration, safe
runtime handover, and Windows `SendInput` receiver path are implemented on the
feature branch. Automated workspace tests cover negotiation, liveness, role
epochs, migration, four-edge geometry, and input cleanup on both build targets.

Live InputCapture and repeated Linux-to-Linux/Windows-to-Linux handover still
need the Phase 0 probe and end-to-end run on the target Hyprland computers; no
Wayland compositor is available in the Windows/WSL development environment.
Phase 6 keeps the old executable names as compatibility entry points because
their deprecation window has not happened yet; UI, help, and examples are now
role-neutral.

## Goal

Keep the existing Windows-controller to Linux-receiver path working while adding:

1. Linux controller to Linux receiver.
2. A later tray action that can make either Linux computer the controller.
3. A role-neutral foundation so Windows receiver support can be added without
   redesigning the protocol again.

On the current Windows/Linux hardware pair, switching Linux into the controller
role requires Windows injection and therefore lands later than Linux-to-Linux
switching. Until that backend exists, the unavailable tray choice is shown but
disabled with a short reason.

This remains a paired, two-computer LAN KVM. Multi-peer routing, discovery,
Internet relays, and automatic mesh behavior are deliberately out of scope.

## Current Shape

The workspace has useful platform separation already:

- `edge-windows-input` captures Windows input.
- `edge-linux-input` injects input on Linux.
- `edge-protocol`, `edge-crypto`, `edge-geometry`, and `edge-keymap` are shared.
- Clipboard traffic is already bidirectional.

The main limitation is that four separate ideas are currently treated as one
role:

- Windows is the controller.
- The controller initiates TCP.
- The controller captures input.
- Linux is the receiver and injects input.

The application binaries, `Hello.role`, pairing text, configuration names, tray
state, audio ownership, and connection loops all assume this arrangement.
Linux-to-Linux therefore needs more than a second capture backend: connection
ownership, active control ownership, capture, and injection must become separate
concepts.

## Selected Architecture

### Separate transport role from control role

Each pair has a stable transport arrangement:

- One device is the **connector**.
- One device is the **listener**.

This only determines who opens the encrypted TCP session. It does not determine
who controls whom. The existing deployment keeps Windows as connector and Linux
as listener, so current firewall and reconnect behavior remain unchanged.

Over an established session, exactly one device is the **active controller** and
the other is the **active receiver**. A tray role swap changes the active
controller without reconnecting or changing IP configuration.

```text
Transport (stable):  connector  <==== encrypted duplex session ====>  listener
Control (switchable): controller  ---------------- input ----------->  receiver
After role swap:      receiver    <---------------- input -----------  controller
```

This is intentionally simpler than making both devices listen, discover one
another, race outbound connections, and deduplicate sessions.

### Treat capture and injection as capabilities

A device advertises what it can do rather than relying on its OS name or startup
role:

- `InputCaptureV1`
- `InputInjectV1`
- `AudioCaptureV1`
- `AudioPlaybackV1`
- future platform-specific optional capabilities

A computer may become controller only if it has capture support. It may become
receiver only if it has injection support. The tray must disable impossible
role choices and explain the missing backend in the status text.

Clipboard remains symmetric and independent of the active input role. Audio also
remains an independently negotiated service; changing who controls input must not
silently reverse or restart audio. A stream starts only when one peer advertises
audio capture and the other advertises audio playback. The existing Linux side
has capture only and the Windows side has playback only. Linux audio playback is
deferred, so Linux-to-Linux sessions show audio as unavailable instead of trying
to negotiate a stream neither side can play.

### Move session orchestration into shared code

Extract the duplicated controller/receiver session machinery from the two large
application `main.rs` files into a new `edge-runtime` crate. It should own only
platform-neutral behavior:

- encrypted session establishment after TCP is supplied;
- hello and capability negotiation;
- pairing and pinned identity handling;
- heartbeat and disconnect supervision;
- role state and role switching;
- input frame scheduling;
- bidirectional clipboard coordination;
- platform-service lifecycle commands and status events.

Platform crates provide small adapters:

```rust
trait InputCapture {
    fn capabilities(&self) -> CaptureCapabilities;
    async fn preflight(
        &mut self,
        layout: &ScreenInfo,
        exit: Edge,
    ) -> Result<CapturePreparation>;
    async fn arm(&mut self, prepared: CapturePreparation) -> Result<()>;
    async fn next_event(&mut self) -> Result<CapturedInput>;
    async fn release(&mut self, reason: ReleaseReason) -> Result<()>;
}

trait InputInjector {
    async fn preflight(&mut self) -> Result<ScreenInfo>;
    async fn inject(&mut self, event: InputEvent) -> Result<()>;
    async fn all_keys_up(&mut self) -> Result<()>;
}
```

The exact async trait implementation can use enums or channels instead of a new
macro dependency. The important boundary is behavior, not the trait syntax.
`CapturePreparation` carries the compositor layout generation or portal
`zone_set` used during preflight. A layout change invalidates it; handover aborts
and preflights again rather than arming stale barriers.

## Linux Capture Backend

### Primary: XDG InputCapture portal plus libei receiver mode

Use the standard `org.freedesktop.portal.InputCapture` flow:

1. Create and start a portal session requesting pointer and keyboard capture.
2. Connect to EIS.
3. Read the compositor-provided zones.
4. Install a barrier on the configured screen edge.
5. Enable the session.
6. When the compositor activates capture, consume events using a libei receiver
   and translate them into the existing `InputEvent` types.
7. On remote return, emergency hotkey, disconnect, backend failure, or shutdown,
   release the portal capture and suggest the correct local cursor position.

This matches the KVM interaction directly: the compositor keeps local input
until the edge barrier is crossed, then routes captured input to the application
instead of the local desktop. It also avoids polling the cursor or taking raw
exclusive ownership of `/dev/input/event*` devices.

The existing libei code is sender-only and uses the RemoteDesktop portal through
liboeffis. Linux capture needs a separate receiver implementation and the
InputCapture portal lifecycle; do not stretch `LibeiBackend` into doing both.
Name the two directions explicitly, for example `PortalCaptureBackend` and
`LibeiInjectionBackend`.

### Hyprland compatibility fallback

If the installed portal does not expose InputCapture, add a backend using
Hyprland's `hyprland_input_capture_v1` protocol. Keep it behind the same
`InputCapture` boundary and select it only after the standard portal path fails.
It should request Hyprland's input-capture permission normally rather than
editing user configuration automatically.

Do not make direct evdev plus `EVIOCGRAB` the normal fallback. It needs elevated
device permissions, can grab the wrong keyboard or mouse, behaves poorly with
hot-plugging, and makes recovery from a crash more dangerous. It can remain a
future explicit advanced backend if another compositor offers no safe capture
API.

Technical references:

- [XDG Desktop Portal InputCapture](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html)
- [libei client libraries and receiver mode](https://libinput.pages.freedesktop.org/libei/libraries/index.html)
- [Hyprland protocol extensions](https://github.com/hyprwm/hyprland-protocols)
- [XDPH release adding InputCapture](https://github.com/hyprwm/xdg-desktop-portal-hyprland/releases/tag/v1.4.0)

### Event translation

- libei key codes are already Linux evdev codes, matching the current wire type.
- Relative pointer motion, buttons, and wheel events map directly to the current
  protocol.
- Track pressed keys and buttons on the controller and receiver.
- Send and apply `AllKeysUp` on every release or failure path.
- Keep the emergency release chord local to the capture backend and never forward
  the chord itself.

The current edge transition field is named `normalized_y`; generalize the next
protocol to `normalized_position`, meaning Y for left/right edges and X for
top/bottom edges. This is an intentional protocol-v2 wire change, not an in-place
rename claimed to be compatible with v1.

## Protocol Evolution

Increment `PROTOCOL_VERSION` to 2 when role-neutral messages and capabilities are
introduced. Both paired applications must be upgraded together; interoperability
with old binaries is not a requirement. What must remain working is the current
Windows-controller to Linux-receiver feature set after both peers are upgraded.

This explicit break is preferable to pretending that adding serde enum variants
is backward compatible. Unknown `Capability`, `ControlEvent`, or `Frame` variants
cannot be skipped by the existing MessagePack decoder.

The break must still be diagnosable during a one-machine-at-a-time update. Keep
the existing `Hello.capabilities: Vec<Capability>` field, always serialize it as
an empty list in v2, and add a separate, defaulted
`Hello.node_capabilities: Vec<NodeCapability>` field. Because named MessagePack
structs ignore an unknown field, a v1 peer can decode the v2 hello, read
`protocol_version = 2`, and report `Upgrade the other computer` instead of dying
on an unknown enum variant. Protocol-v2 typed node capabilities are:

- `InputCaptureV1`
- `InputInjectV1`
- `RoleSwitchV1`
- `ScreenInfoBothSidesV1`
- `AudioCaptureV1`
- `AudioPlaybackV1`

After confirming protocol v2, both peers read only `node_capabilities` for
feature decisions and ignore the legacy `capabilities` field. The old field is a
decode-shape shim, not a second source of truth for audio or any other feature.

Reserve `extensions: Vec<String>` for optional behavior that uses wire messages a
decoder already understands. Do not use strings merely to hide incompatible new
frame shapes. A later unknown enum variant requires another protocol bump unless
the protocol first gains a deliberately opaque extension envelope.

No v2-only `Frame` or `ControlEvent` is sent until both hello messages have been
decoded and version 2 has been confirmed. Treat a version mismatch as a
non-transient connection error: show the required-upgrade status and stop
automatic retry until the user requests reconnect or restarts the upgraded app.
Do not emit the generic two-second reconnect log loop for this condition.

`Hello.role` remains only to preserve the v1-decodable hello shape. In v2 it is
informational status text reflecting the sender's local preference; no role,
transport, authorization, or handover decision may branch on it. Startup
selection uses the connector's local `preferred_role` and the authoritative
session state described below. Both devices send `ScreenInfo` after hello so
either side is ready for a later role swap.

Add a role state message containing:

- stable controller identity (the paired fingerprint is sufficient initially);
- monotonically increasing role epoch;
- current transition state;
- connector-authoritative topology, including `listener_position`;
- connector-authoritative runtime pause flag;
- optional failure detail.

The connector sends topology in the initial v2 session state. The listener keeps
it for the session and, when promoted to controller, uses the opposite edge as
its exit. The listener does not invent or persist a competing position. A
topology change is accepted only while capture is disarmed and increments the
role/session-state epoch.

The connector also sends the pause flag in every initial session state. The
listener adopts it unconditionally, using the same authority rule as topology
and committed role.

Use a small two-phase handover coordinated by the transport connector:

1. `Prepare`: stop new edge activation on the old controller.
2. Old controller releases local capture and sends `AllKeysUp`.
3. Proposed receiver preflights its injection backend.
4. Proposed controller preflights its capture backend but leaves it disarmed.
5. Both report ready.
6. `Commit`: increment the role epoch, then arm the new controller only if input
   forwarding is enabled.

If either preflight fails, abort and restore the previous assignment. Frames with
a stale epoch are ignored. On disconnect, both sides release capture, inject
`AllKeysUp`, and return to normal local input.

Only the transport connector assigns epochs. A role request made from the
listener tray is sent to the connector for serialization. This avoids a bulky
distributed-election mechanism for a two-device application.

### Reconnect and crash authority

The connector is authoritative for the committed controller identity:

- It stores the committed controller fingerprint atomically in
  `state/role.toml` before transmitting `Commit`.
- The listener mirrors the same value into its own `state/role.toml` after
  accepting `Commit`; disagreement is still resolved in favor of the connector.
- A tray role switch never rewrites `preferred_role` or any other user-edited
  configuration. `preferred_role` is consulted only when no valid committed role
  state exists.
- At the next hello, the connector announces its persisted controller identity,
  starts a fresh session epoch, and the listener adopts it even if its local file
  disagrees after a crash.
- A prepared but uncommitted transition is discarded on reconnect.
- Replacing a paired identity clears role state that refers to the old
  fingerprint and returns the pair to fresh-pair selection.
- A listener-initiated request has a bounded response deadline. If the connector
  does not answer, the listener aborts the request, retains the current role, and
  restores normal tray controls.

### Fresh-pair role selection

On a pair with no committed controller, the connector is also the startup
authority:

1. Its `preferred_role` selects the initial controller: `controller` selects the
   connector and `receiver` selects the listener.
2. The listener's copied or conflicting preference is ignored.
3. If that direction lacks either controller capture or receiver injection, try
   the opposite direction when its capabilities are complete.
4. If neither direction is valid, keep the authenticated session alive for
   clipboard and other symmetric services, leave input capture disarmed, and show
   `No compatible input direction` on both trays.

### Input-forwarding state

`Forward mouse and keyboard` remains a connection-scoped state coordinated by
the connector, matching the current behavior:

- A role handover preserves its current value.
- `Prepare` always disarms capture temporarily without changing that preference.
- After `Commit`, the new controller arms capture only when forwarding is enabled.
- If forwarding is disabled, both sides remain locally usable and the new role is
  committed but dormant.
- Reconnect continues to default forwarding to enabled unless a later feature
  explicitly makes the preference persistent.

### Symmetric liveness

Move heartbeat production and supervision into `edge-runtime`; neither active
role may rely on a platform application's historical behavior:

- Run liveness supervision in a dedicated runtime task that never awaits input,
  clipboard, audio, or UI work.
- While remote input is active or a role transition is running, both peers send
  an authenticated heartbeat every 250 ms, scheduled ahead of bulk clipboard
  traffic. When no input direction is active, reduce this to one heartbeat per
  second.
- Any valid authenticated inbound frame refreshes `last_peer_activity`.
- If no inbound activity arrives for 1 second while remote input is active, the
  controller disarms local capture and the receiver injects `AllKeysUp`, but the
  encrypted session remains connected. Mark that input epoch suspended and
  ignore late buffered input until a fresh `EnterRemote` for the committed epoch
  explicitly reactivates it.
- If silence reaches 5 seconds, tear down the session and apply the normal
  reconnect policy. This preserves the working path's current hard-failure
  tolerance while making held-input release eager and idempotent.
- `TCP_USER_TIMEOUT` remains a supplemental unreachable-peer safeguard, not the
  application input-release watchdog.
- Before fixing these as release constants, send a maximum-size 4 MiB clipboard
  image through a throttled, small-buffer test transport and record the longest
  complete-frame gap. The one-second soft and five-second hard defaults require
  margin over that measured gap; any adjustment must retain separate soft-release
  and hard-disconnect deadlines.
- Tests use paused Tokio time to prove both role directions soft-release at one
  second, ignore stale buffered input, recover on a fresh entry, and hard-disconnect
  at five seconds when the peer stays connected but stops producing frames.

## Configuration Migration

Move toward role-neutral names while accepting the existing files:

```toml
device_name = "Desk PC"
start_with_windows = false
preferred_role = "controller"
transport = "connect" # or "listen"
listen = "0.0.0.0:42420"

[peer]
name = "Laptop"
host = "192.168.0.11"
port = 42420
pinned_fingerprint = "..."

[layout]
# Stable topology: listener is left of connector, regardless of active role.
listener_position = "left"

[input.capture]
backend = "auto"
output = ""
release_hotkey = "Ctrl+Alt+Pause"
game_compatibility = "always-enabled"

[input.inject]
backend = "auto"
output = "eDP-1"

# Existing [clipboard] and [audio] sections carry over unchanged.
```

Migration rules:

- Existing `role = "controller"` implies `preferred_role = "controller"` and
  `transport = "connect"`.
- Existing `role = "receiver"` implies `preferred_role = "receiver"` and
  `transport = "listen"`.
- Existing `[peer.laptop]` is accepted and migrated to `[peer]` when settings are
  next saved.
- Existing `[peer.laptop].position` becomes `layout.listener_position` on the
  connector. The listener's relative edge is derived as the opposite direction
  after a role swap; active role never changes stored physical topology.
- Existing `monitor` becomes `input.inject.output`. Linux capture defaults to the
  same output when `input.capture.output` is empty.
- Existing `input.game_compatibility` becomes
  `input.capture.game_compatibility`; it remains a Windows-capture setting.
- Existing clipboard, audio, Windows startup, device name, network, and pinned
  identity values are preserved during the rewrite even when an abbreviated
  configuration example omits them.
- `preferred_role` is a user-owned first-pair default only. Committed tray
  switches are stored in portable `state/role.toml`, never written back into the
  editable config.
- Every platform default includes an emergency release hotkey even when its
  preferred startup role is receiver.
- Config, identity, restore tokens, and logs stay beside the executable unless an
  existing override flag or environment variable is used.

`input.capture.output` is Linux-only in this plan. An empty value lets the
compositor choose the default capture zone. Windows keeps its current behavior of
using the complete virtual desktop, does not show this setting in its UI, and
warns then ignores a manually configured non-empty value. Windows per-monitor
capture selection is deferred until it has an implementation.

The schema-v2 migration is one-way. Before the first rewrite, create one adjacent
`*.v1.bak` copy atomically; do not keep writing duplicate old and new keys. A
downgrade can restore that backup manually but is otherwise unsupported.

Do not automatically swap `transport` when the user changes the active role.

## Applications and Tray

Avoid immediately replacing both working binaries. First make them thin shells
around `edge-runtime`, then introduce role-neutral platform applications:

- `edge-kvm-linux`
- `edge-kvm-windows`

During migration, keep `edge-controller-win` and `edge-receiver-linux` as
compatible entry points or package aliases. Remove the old names only in a later
explicit cleanup release.

Right-click tray menu:

```text
Role
  (o) This PC controls Peer Name
  ( ) Peer Name controls this PC
-----------------------------
Forward mouse and keyboard
Disconnect
Pair or replace peer...
Settings...
Quit
```

`Disconnect` is state-dependent: while disconnected, paused, or blocked by a
version mismatch, the same menu position reads `Reconnect`. That manual action
is how a user retries after upgrading the other computer.

For a compatible paired session, `Disconnect` means a non-persistent runtime KVM
pause, not deletion of pairing or a promise of zero control traffic:

- Send the pause over the authenticated session, disarm capture, inject
  `AllKeysUp`, and stop clipboard/audio services on both peers.
- Keep, or automatically re-establish, only the lightweight encrypted
  control/heartbeat channel so `Reconnect` on either tray can resume the pair.
- The connector owns the in-memory pause flag. It survives transport reconnects
  and listener restarts; a connector restart clears it and the listener adopts
  the cleared value from the next initial session state.
- Disable role switching while paused.

This prevents the connector from immediately restoring active KVM services after
the listener clicks `Disconnect`, while still allowing either computer to resume
without an out-of-band wake-up mechanism.

The selected item reflects committed role state, not a speculative click. While
handover is running, disable both choices and show `Switching role...`. If the
switch fails, retain the prior selection and show one concise error.

Settings should use `Controller` and `Receiver` only where they describe the
current role. Pairing and status strings should say `peer` or the device name,
not `laptop`, `Windows`, or `Linux`.

## Implementation Phases

### Phase 0: Protect the working baseline

- Record end-to-end checks for the current Windows to Linux path.
- Add a protocol fixture for the current v1 hello and control frames.
- Add a prospective v2-hello fixture proving the current v1 decoder can ignore
  `node_capabilities`, reach `protocol_version = 2`, and produce an actionable
  version mismatch. Keep v2-only enum values out of v1-known hello fields.
- Add tests proving legacy controller and receiver configs still load.
- Record that protocol v1 and v2 binaries intentionally do not interoperate.
- On the target Linux system, probe the installed InputCapture portal interface,
  its supported keyboard/pointer capabilities, `libei-1.0`, and the installed
  XDPH version. Use the result to decide whether the standard portal or direct
  Hyprland backend is implemented first.
- If neither capture API exists, stop the Linux-controller milestone with an
  actionable Hyprland/XDPH upgrade requirement. Do not silently promote raw
  evdev capture; an explicit opt-in evdev backend requires a separate decision
  because of device permissions and crash-recovery risk.
- Make no behavior or config changes in this phase.

Exit: current Windows controller to Linux receiver behavior is reproducible and
covered against accidental protocol/config breakage.

### Phase 1: Extract role-neutral runtime seams

- Generalize `NoiseSession::split` from the current `TcpStream::into_split`
  implementation to `tokio::io::split` over generic
  `AsyncRead + AsyncWrite + Unpin` streams. Make `NoiseReader` and `NoiseWriter`
  generic so TCP production sessions and in-memory duplex sessions execute the
  same encrypted read/write path.
- Add an `edge-runtime` skeleton generic over `AsyncRead + AsyncWrite` and fake
  capture/injection/clipboard adapters.
- Replace the current one-way heartbeat arrangement with the shared symmetric
  adaptive heartbeat, one-second soft input-release deadline, and five-second
  hard session deadline before extracting input ownership. Test a
  connected-but-silent peer in both controller directions and verify receiver
  `AllKeysUp`, controller capture release, stale-input suspension, and eventual
  session teardown.
- Measure complete-frame gaps during a 4 MiB image transfer over a throttled,
  small-buffer transport before locking the liveness constants.
- Before moving the production state machines, add an in-process integration
  harness over `tokio::io::duplex` covering pre-trusted hello, screen info, input,
  clipboard, heartbeat timeout, disconnect cleanup, and `AllKeysUp`.
- Add a second loopback case for untrusted hello, injected accept/decline pairing
  decisions, and fingerprint persistence in a temporary directory; the test must
  never open a real consent window.
- Move pairing, hello, heartbeat, scheduled writes, disconnect handling, and
  shared clipboard session logic behind that harness incrementally; the harness
  must stay green after each move.
- Introduce capture/inject adapters without changing the current direction.
- Rename internal variables from `laptop`/`controller`/`receiver` to `peer`,
  `local`, and `remote` where they represent topology rather than active role.
- Move the generic four-edge calculations already present in
  `edge-windows-input` into `edge-geometry`; the currently narrow
  `enter_left_edge`/`leave_right_edge` helpers are not the full implementation.
- Keep binaries, defaults, TCP direction, and UI behavior unchanged.

Exit: Windows-to-Linux passes its live baseline, and the new loopback runtime test
passes with thin application orchestration.

### Phase 2: Linux capture proof of life

- Implement the backend selected by the installed-system Phase 0 probe. Prefer
  InputCapture portal plus libei receiver mode when the installed XDPH exposes it;
  otherwise implement the direct Hyprland capture protocol first and retain the
  portal as the cross-compositor default.
- Add `--test-capture` that displays counts and event categories without logging
  keys or typed text.
- Verify edge activation, pointer deltas, buttons, wheel, keyboard, release chord,
  portal revocation, device hot-plug, and clean release.
- Implement the other path as the fallback after the target backend is proven.

Exit: the Linux machine can safely capture at an edge and always recover local
input without a network peer.

### Phase 3a: Protocol and config flag day

- Upgrade the existing Windows-controller/Linux-receiver pair with no input-role
  behavior change:
  - bump the wire protocol to v2;
  - add the legacy-decodable hello envelope and `node_capabilities`;
  - define all v2 wire shapes, including role state/handover messages,
    connector-authoritative topology, and `normalized_position`, even though live
    handover waits for Phase 4;
  - have both peers advertise screen information and directional input/audio
    capabilities through `node_capabilities`;
  - apply the one-time schema-v2 migration and create `*.v1.bak`;
  - classify version mismatch as `Upgrade the other computer` and suspend
    automatic reconnect until manual retry or restart.
- Do not enable Linux capture over the network or runtime role switching in this
  phase.
- Re-run the Phase 0 Windows-to-Linux baseline using migrated real-world config,
  including input, clipboard, audio, pairing, reconnect, and recovery tests.
- Explicitly verify v2 edge entry and return landing positions on left, right,
  top, and bottom placements using mismatched local/remote resolutions. Compare
  normalized positions against the v1/shared-geometry fixtures so a silent cursor
  offset regression blocks Phase 3a.

Exit: the fully upgraded Windows/Linux pair behaves as it did under v1, the
backup and migrated config are verified, and any regression can be attributed to
the isolated wire/config flag day.

### Phase 3b: Linux controller to Linux receiver

- Build the Linux controller path on `edge-runtime` using the proven capture
  adapter and v2 session.
- Reuse the existing Linux injection backends on the receiving machine.
- Keep the Linux-to-Linux audio action disabled because Linux playback is
  deferred.
- Keep a fixed startup role and the existing connector/listener transport model.
- Update Linux tray state and pairing wording to be peer-neutral.
- While remote modifiers or buttons are held, revoke the Linux controller's
  compositor capture session and verify the runtime immediately sends
  `AllKeysUp`, the receiver releases every held input, and both local desktops
  remain usable.

Exit: two Linux/Hyprland machines pair, reconnect, cross the configured edge in
both directions, and transfer mouse, keyboard, and clipboard reliably. The
Phase 3a Windows-to-Linux baseline remains green.

### Phase 4: Safe runtime role switching

- Activate the already-defined v2 role handover: epochs,
  prepare/ready/commit/abort handling, and stale-frame rejection.
- Add the two radio-style right-click role actions on both peers.
- Persist only committed role assignments.
- Preserve the connection-scoped input-forwarding value across every handover.
- Test failures at every handover point and simultaneous tray requests.

Exit: either Linux computer can make itself controller or receiver without
reconnecting, trapping input, or leaving pressed keys behind.

For a Windows/Linux pair at this milestone, `Linux controls Windows` remains
disabled because Windows cannot inject yet. This is intentional: the requested
cross-platform switch arrives in Phase 5, described as down-road work.

### Phase 5: Windows receiver support

- Add a Windows `InputInjector` using `SendInput` with explicit evdev-to-Windows
  mapping and injected-event tagging so the capture hooks ignore self-injected
  input.
- Make the Windows application role-neutral and expose the same tray choices.
- Verify that UAC/secure desktop remains an explicit unsupported boundary.

Exit: a paired Windows/Linux pair can switch controller direction at runtime.

### Phase 6: Packaging cleanup

- Ship role-neutral executable names once compatibility entry points have had a
  deprecation window.
- Update examples and README to describe devices and roles instead of fixed OS
  directions.
- Keep portable per-binary storage and provide an explicit migration command if
  executable renaming changes the containing directory.

## Safety Invariants

- Exactly one active controller exists per session epoch.
- The runtime enforces the testable implication `capture armed => local role is
  controller => local epoch equals the last committed epoch => the remote
  injector reported ready for that epoch`.
- Capture is never active without a healthy authenticated session and a ready
  remote injector.
- A role transition cannot commit until the previous controller has released.
- Disconnect, heartbeat timeout, portal revocation, injector failure, switch
  abort, and shutdown all release capture and apply `AllKeysUp`.
- Capture revocation while inputs are held triggers an immediate remote
  `AllKeysUp`; if that write cannot complete, the symmetric inbound-activity
  watchdog releases receiver-held input no later than one second after the last
  authenticated frame.
- A one-second liveness lapse suspends only the input epoch and ignores late input;
  it does not tear down an otherwise recoverable session. Five seconds of silence
  is the hard disconnect threshold.
- The emergency release chord is processed locally on every capture platform.
- Role switching never modifies firewall rules, OS config directories, Hyprland
  config, udev rules, or device permissions automatically.
- Clipboard and audio failures cannot terminate input recovery handling.

## Tests and Acceptance

### Shared tests

- Legacy and new config migrations and round trips.
- Protocol-v2 capability negotiation for each supported capture/injection/audio
  combination, including unavailable-role explanations.
- Protocol-v1/v2 mismatch fails closed without arming capture; mixed-version
  operation itself is not supported.
- A v1 decoder reaches the version field of a v2 hello, and protocol mismatch is
  classified as a non-retrying upgrade error rather than a generic decode error.
- Fresh-pair startup selection covers conflicting preferences, missing capture,
  missing injection, reverse-direction fallback, and clipboard-only fallback.
- Connected-but-silent peers soft-timeout symmetrically: the controller disarms
  capture and the receiver applies `AllKeysUp` within one second under paused
  Tokio time, late input is ignored until fresh entry, and the session remains up
  until the five-second hard deadline.
- A throttled 4 MiB clipboard transfer records the maximum complete-frame gap and
  does not spuriously cross either measured liveness threshold.
- Connector topology reaches the listener in initial session state, reverses the
  exit edge when the listener controls, and rejects stale-epoch updates.
- Pause state survives a transport reconnect and listener restart, while a
  connector restart clears it for both peers on the next initial session state.
- Role prepare/commit/abort, stale epochs, duplicate messages, and conflicting
  requests.
- Disconnect or timeout at every handover step.
- Shared geometry extracted from the Windows backend covers all four edge
  directions and differently scaled outputs.
- `AllKeysUp` and pressed-button cleanup are idempotent.

### Linux backend tests

- Portal capability absence and user denial produce useful errors.
- Barrier and zone rebuild after monitor layout changes.
- A zone/layout change between capture preflight and commit invalidates the
  preparation token and aborts or repeats handover before capture can arm.
- Activation and release preserve the correct normalized edge position.
- Emergency chord is consumed locally.
- Backend revocation immediately restores local input.
- Backend revocation while the remote has held modifiers/buttons produces remote
  `AllKeysUp` before the input direction is torn down.

### End-to-end matrix

| Controller | Receiver | Required milestone |
|---|---|---|
| Windows | Linux | Must work after both peers are upgraded in every phase |
| Linux | Linux | Phase 3b |
| Linux | Windows | Phase 5 |
| Windows | Windows | Later validation once Windows injection exists |

For every supported row verify edge enter/return, click, wheel, normal typing,
modifiers, clipboard both ways, reconnect, peer process kill, and network loss.
Where role switching is available for that row and milestone, also verify
repeated role switches.

## Deferred

- More than two computers in one topology.
- Automatic LAN discovery.
- Multiple simultaneous receivers.
- Internet/NAT traversal.
- macOS capture or injection.
- Direct evdev capture as a default backend.
- Reversing audio direction merely because the input role changed.

## Recommended First Deliverable

Before editing Rust, run and record the Phase 0 portal/libei/Hyprland capture
probe on the target Linux machine. Its result selects the Phase 2 backend or
stops the milestone with the documented compositor-upgrade requirement.

Implement Phases 0 through 2 as one bounded milestone: preserve and extract the
working runtime, then prove safe Linux edge capture locally. Do not begin network
role switching until Linux capture can be activated and released repeatedly
without trapping the keyboard or pointer. That proof removes the largest
platform risk before changing the stable protocol path.
