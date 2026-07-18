# Linux local cursor handoff

## Summary

When Windows enters remote control, hide the composited cursor on the Linux receiver. Physical
mouse movement or touchpad contact on Linux immediately restores the cursor and hands control
back to the Linux user.

Hyprland exposes one composited cursor, so hiding it also hides the cursor driven by remote input.
This proposal intentionally treats local physical activity as an ownership handoff instead of
allowing local and remote pointer streams to compete.

## User experience

1. Windows crosses the configured edge and sends `EnterRemote`.
2. Linux confirms that local pointer monitoring is healthy, then sets
   `cursor:invisible = true` through `hyprctl keyword`.
3. Remote input continues normally with the Linux cursor hidden.
4. Moving a physical Linux mouse, pressing one of its buttons, scrolling it, or touching/moving
   the touchpad immediately:
   - restores the Linux cursor;
   - releases injected buttons and keys;
   - sends `ReleaseToLocal { reason: LocalInput }` to Windows;
   - stops accepting remote movement until Windows enters again normally.
5. Disconnects, backend failures, manual disconnects, and receiver shutdown always restore the
   cursor setting that existed before edge-kvm changed it.

The feature is fail-visible: if physical-device monitoring or cursor control is unavailable, the
receiver logs the reason and leaves the cursor visible.

## State model

| State | Cursor | Remote input | Local pointer activity |
| --- | --- | --- | --- |
| `Local` | Previous user setting | Not active | Normal |
| `RemotePending` | Visible | Active | Cancels remote ownership |
| `RemoteHidden` | Hidden | Active | Restores cursor and releases remote ownership |
| `Recovering` | Restoring | Releasing keys/session | Ignored until restoration completes |

`RemotePending` provides a short delay, initially 100 ms, so an `EnterRemote` immediately followed
by a release does not flash or leave the cursor hidden.

## Architecture

### `LinuxCursorController`

Add a Hyprland cursor controller to `edge-linux-input` that:

- reads the current `cursor:invisible` value with `hyprctl getoption`;
- changes it with `hyprctl keyword cursor:invisible true|false`;
- makes hide/show idempotent;
- restores the original value from an explicit shutdown call and a best-effort `Drop` guard;
- never changes the setting when the original value was already hidden.

Commands must use `tokio::process::Command` arguments directly, without shell interpolation.

### `LocalPointerActivityMonitor`

Add a Linux-only monitor that observes physical pointer devices without grabbing them:

- discover only udev devices tagged `ID_INPUT_MOUSE=1` or `ID_INPUT_TOUCHPAD=1`;
- obtain read-only device file descriptors through an installed udev `uaccess` rule that matches
  only mouse and touchpad event devices, avoiding membership in the security-sensitive `input`
  group and avoiding logind session-controller conflicts with the compositor;
- monitor mouse relative motion, pointer buttons, and wheel events;
- monitor touchpad contact and meaningful absolute movement;
- ignore keyboards and all unclassified devices;
- handle device hotplug and loss of device access;
- debounce touchpad noise and coalesce activity into a bounded Tokio channel.

The WLR virtual pointer used by edge-kvm is not an evdev device and therefore cannot trigger this
monitor. Device identity must still be asserted in tests so a future backend cannot accidentally
wake itself.

The receiver must not recommend granting broad `/dev/input` keyboard access. If the pointer-only
udev rule is not installed or active, initialization fails visibly and the cursor remains visible.

### `LinuxCursorHandoff`

Add a coordinator owned by the active receiver session. It consumes:

- controller `ControlEvent`s;
- local pointer activity;
- connection/backend shutdown notifications.

The coordinator is the only code allowed to hide the cursor. On local activity while remote is
active it must show the cursor before sending the release frame, call `backend.all_keys_up()`, clear
the remote return watcher, and emit `ReleaseToLocal { reason: LocalInput }`.

### Protocol and Windows behavior

Append `LocalInput` to `ReleaseReason` and update the Windows receiver-release policy to accept it
without requiring the remote cursor to be at the configured return edge. Increment the protocol
version because older controllers cannot decode the new reason reliably.

Windows already restores its source cursor when a receiver release is accepted. Add explicit logs
and a counter for local-input handoffs.

## Configuration

Add receiver configuration with conservative defaults:

```toml
[input.local_cursor_handoff]
enabled = true
hide_delay_ms = 100
wake_on_mouse = true
wake_on_touchpad = true
```

`enabled = true` is safe only because hiding is conditional on both the activity monitor and cursor
controller reporting ready. Unsupported compositors and degraded monitors remain visible.

Expose readiness and the last handoff reason in the tray tooltip/settings diagnostics. Do not add
dynamic rows to the right-click menu.

## Implementation sequence

1. Add `LinuxCursorController` with parser, command-runner, idempotency, and restoration tests.
2. Add physical-device discovery and pointer-only udev-backed activity monitoring.
3. Add the handoff state machine with a fake cursor controller and synthetic activity tests.
4. Integrate `EnterRemote`, release, disconnect, and shutdown paths in the Linux receiver.
5. Add `ReleaseReason::LocalInput`, bump the protocol version, and update Windows acceptance/logging.
6. Add configuration, settings diagnostics, counters, and portable logs.
7. Perform Hyprland integration tests with a USB mouse and the built-in touchpad.

Keep these as reviewable commits; do not combine device access, cursor mutation, and protocol
changes into one untestable change.

## Verification

### Automated

- state transitions never hide before monitor readiness;
- local activity in `RemoteHidden` restores exactly once and emits one release;
- remote virtual-pointer events do not count as local activity;
- disconnect/error/shutdown restore the original cursor setting;
- an originally hidden cursor remains hidden after the session;
- device hotplug and loss of device access do not strand the cursor;
- touchpad noise below threshold does not trigger a handoff;
- protocol round trips include `LocalInput` and reject mismatched protocol versions cleanly;
- existing edge-return and manual release behavior remains unchanged.

### Manual acceptance

- crossing from Windows hides both the Windows source cursor and Linux composited cursor;
- remote click, scroll, keyboard, clipboard, and return-edge behavior still work;
- physical mouse movement on Linux restores local control in under 100 ms;
- touchpad contact/movement restores local control in under 100 ms;
- unplugging the active pointer, killing the controller, stopping the receiver, and forcing an
  input-backend error all leave the Linux cursor visible;
- restarting the receiver preserves the user's original `cursor:invisible` preference.

## Risks and mitigations

- **Cursor stranded hidden:** hide only after readiness and restore on every terminal path with a
  best-effort guard.
- **Two active pointer owners:** local activity performs a protocol-level release, not only show.
- **Touchpad noise:** require contact or movement above a tested threshold and debounce events.
- **Input privacy:** open only udev-classified pointer devices; never inspect keyboard devices.
- **Hyprland-specific behavior:** isolate compositor commands behind a trait and use a visible no-op
  implementation elsewhere.
- **Version skew:** bump protocol version and present a clear upgrade error rather than silently
  ignoring `LocalInput`.

## Open review questions

1. Should local mouse buttons and wheel movement summon local control, or only pointer movement?
2. Should touchpad contact alone summon control, or require measurable movement?
3. Is 100 ms an acceptable hide delay, or should it be configurable but default to immediate?
4. Should the feature initially be opt-in for one release despite the fail-visible readiness gate?
