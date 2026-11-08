# Basic focused-window remote view

The `phone-window-view-v1` extension adds a cropped JPEG preview and explicit
touch/key actions to the existing encrypted phone connection. It is a low-rate
prototype, not a video codec or a full remote desktop implementation.

## Try it

Build the matching receiver and HyprMobile feature branches. The receiver needs
`grim`, the Hyprland session environment, and Hyprland's Lua `eval` API for input.
Window capture works with logical desktop coordinates, including scaled and
rotated monitors. Only the window's intersection with its assigned monitor is
captured. Menus outside that crop and hidden/occluded window contents are not
captured separately.

New pairings grant `remoteview` and `remoteinput`. Existing pins retain their
grants; re-pair or intentionally grant the new features while the receiver is
stopped. `windowfocus`, `remoteview`, `remoteinput`, and `clipboard` are distinct
permissions. Clipboard permission is required for the text composer's paste.

On the phone, choose **Live view**, then **Touch on** when you want input.
Tap to left-click, arm **Right click…** for a single right click, use the key
buttons, or compose text and choose **Paste text**. Terminal paste uses
Ctrl+Shift+V. Pasting intentionally replaces the desktop clipboard. Fit/2× zoom
and touch panning help with small targets. There is no drag or desktop scroll
gesture yet; the arrow keys provide basic navigation.

## Transport and bounds

- Both peers negotiate the extension; old phones never receive new events.
- A monotonically increasing `view_id` identifies each start/stop/control-mode
  request within a connection. Frame IDs are scoped to that view.
- Each image includes its target window, actual logical crop, encoded dimensions,
  timestamp, and an `interactive` flag. JPEG bytes use MessagePack binary encoding.
- The longest encoded edge is at most 960 pixels; JPEG payloads are at most
  48 KiB. Oversized captures are retried at smaller sizes. The complete event
  fits one Noise message, tested with a maximum-length Unicode title.
- The receiver sends at most two frames per second. It waits for the phone's
  image-onload acknowledgement before another capture. Four seconds without an
  acknowledgement closes the stream. There is no accumulating video queue.
- Capture runs separately from the phone's heartbeat/clipboard/audio loop.
  Process output, IPC reads, action queues, and capture duration are bounded.

## Input and lifecycle

Touch is off by default and needs a separate grant. Only one phone can hold the
control lease. The desktop tray's control state must be available, and input is
refused while another KVM computer controls this node or a role switch is in
progress. No generic `Input` or role-control frames are admitted from phones.

Actions refer to a displayed frame. Unknown frames, frames older than two seconds,
and unsettled geometry are rejected. Input is initially disabled until geometry
has stayed unchanged for one second. Capture rechecks window identity and bounds
before and after taking the image. Input rechecks identity, monitor, workspace,
and bounds, then runs fixed Lua guards on Hyprland's event loop. Pointer
motion is sent before the click, with a brief settle and another target check;
key events target the selected window directly. Addresses must be hex; coordinates are bounded; keys come from an enum.
Phone-supplied text is written through the clipboard API, never interpolated
into shell or Lua code. Every click/shortcut is a complete press/release; no
persistent button or modifier holds are created.

Capture and input refuse a locked desktop or an unknown lock state. The phone
closes the view when hidden, destroyed, disconnected, or screen-off. Reconnect
does not resume capture or input. View IDs prevent messages from an earlier view
from becoming input to a newer one.

Hyprland IPC reports layout goals rather than rendered animation coordinates.
The settle interval is conservative for ordinary animations but is not a
frame-perfect animation contract. Long/custom animations, overlapping windows,
and popups remain limitations of this prototype. Window-only capture and
compositor-native frame/crop tracking are the next path to smoother interaction.

## Validation

```sh
cargo test -p edge-protocol -p edge-crypto -p edge-receiver-linux
cargo clippy -p edge-protocol -p edge-crypto -p edge-receiver-linux --all-targets -- -D warnings
# Read-only check in a running session:
cargo test -p edge-receiver-linux live_cropped_capture -- --ignored --nocapture
```

Tests cover wire size, action round trips, capability and permission gating,
acknowledgement backpressure, stale/unsettled frame rejection, negative origins,
coordinate limits, and generated Lua guards without phone-text interpolation.
