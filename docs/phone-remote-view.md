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

On the phone, open the **Remote** tile, then switch **Touch** on when you want input.
Tap to left-click, arm **Right** for a single right click, use **Super + arrows**
to move between windows, or open the keyboard panel to compose text and paste. Terminal paste uses
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
- With `phone-window-view-chunks-v1` (both peers advertise it), the phone
  reports its on-screen frame in device pixels (`RemoteViewSize`, again on
  rotation). Capture then covers that frame, as the phone's Fill mode does, but
  never exceeds the window's native pixels; the longest edge is at most 4096
  and a JPEG at most 512 KiB. Frames over 48 KiB are sent as ordered
  `RemoteViewFrameChunk`s of at most 56 KiB, followed by the `RemoteViewFrame`
  with an empty `jpeg`. Measured on a 1154×1428 window: about 115 KB per frame,
  about 235 KB/s at 2 fps, about 2% of one CPU core on the desktop.
- With `phone-window-video-v1` as well (experimental; advertised only when
  wlr-screencopy works and GStreamer has `nvh265enc`), the view is an HEVC
  stream instead. The window's region is copied with wlr-screencopy into a
  reused shared-memory buffer, and NVENC takes the BGRx directly, with no
  screenshot tool, image file or software conversion. Capture waits for
  damage, so a still window sends one frame every 500 ms (which keeps taps
  valid). One access unit per frame is sent as `RemoteVideoFrame` (chunked
  when large) with the same metadata and input rules. With
  `phone-window-video-modes-v1`, `RemoteVideoMode` switches between Text mode
  (up to 10 fps, constant quality 20, one frame in flight) and Media mode (up
  to 60 fps, ultra-low-latency, quality 26 capped at 20 Mbit/s, three frames
  in flight). Media mode measured 45–55 fps at 16–22 Mbit/s, with about 6 ms
  capture, 2–3 ms encode and 8% of one CPU core. The phone can ask for a keyframe with
  `RemoteVideoKeyframe`. Measured on an 874×1428 text window: a 325 KB first
  frame, then about 1 KB per frame; about 24 KB/s at 9 fps, about 9% of one
  CPU core. Each view logs its fps, data rate, capture/encode time and ack
  time every 10 seconds.
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
and bounds. Pointer motion uses the existing Wayland virtual input backend,
with a compositor roundtrip and another focus/geometry check before clicking.
This preserves client pointer coordinates. Key events use fixed Lua guards and
target the selected window directly. Addresses must be hex; coordinates are bounded; keys come from an enum.

Super+arrow (`super_left`/`right`/`up`/`down`) is the exception. A key sent to
one window would bypass compositor binds, so these are pressed on the virtual
keyboard after the same identity and freshness checks, and the user's own
Super+arrow binds run (focus navigation by default). The receiver advertises
this as `phone-window-navigation-v1`; phones send these keys only when it is
present, because older receivers reject unknown keys. The view then follows
the newly focused window.
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
