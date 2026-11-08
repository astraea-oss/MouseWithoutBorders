# Focused-window metadata for HyprMobile

This is the first step toward a phone view cropped to the focused Hyprland
window. It sends metadata over the existing encrypted phone session. It does
not capture video, resize windows, change focus, or grant input access.

## Trying it

1. Build and run `edge-receiver-linux` from `feature/phone-window-focus` inside
   the Hyprland session. It needs that session's `XDG_RUNTIME_DIR` and
   `HYPRLAND_INSTANCE_SIGNATURE` environment variables.
2. Use HyprMobile's companion `feature/focused-window-follow` branch.
3. Existing pairings do not automatically receive the new `windowfocus`
   permission. Re-pair using `edge-receiver-linux pair-phone` to grant it.
   Newly paired phones receive it alongside the existing phone features.
4. On the phone, press **Follow** in **Focused window**. Switch windows using
   your usual Hyprland/scrolling-layout bindings. The phone shows the window
   title, app, workspace, bounds, and highlighted region of its monitor.

Tracking is off until requested. The app unsubscribes when hidden or when its
screen turns off, and clears tracking on disconnect. Reconnection requires a
new explicit Follow action. Stop following cancels the desktop tracker.

## Wire contract

Both peers advertise `phone-window-focus-v1` in `Hello.extensions`. Existing
closed capability lists and PC input-role messages are unchanged. A phone
without this extension never receives these new events.

- `PhoneEvent::WindowFocusSubscribe { enabled }`: phone to desktop. Requires
  negotiation; enabling also requires the pinned phone's `windowfocus` grant.
- `PhoneEvent::WindowFocusUpdate { sequence, sampled_at_ms, state }`: desktop
  to phone. The initial snapshot is sent immediately; later samples are sent
  only when state changes. Sequence starts at 1 per subscription. The timestamp
  is desktop Unix milliseconds, not a synchronized video timestamp.
- State is `focused` with a `FocusedWindow`, `no_window`, or `unavailable` with
  an explanation. Missing Hyprland, IPC failures, monitor changes, and denied
  permission are handled without breaking clipboard/audio/liveness.

Window metadata contains address, bounded app ID/title, workspace ID, logical
bounds, bounds intersected with its monitor, and monitor ID/name/geometry.
Monitor native pixel dimensions, scale multiplied by 1000, and Wayland transform
are included. Origins can be negative. `visible_bounds: None` means the window
is entirely outside its associated monitor's visible area.

All rectangles use **global desktop logical coordinates**. They must not be
treated as encoded-video pixels. Output scale and rotation must be applied by
the future capture backend. Windows spanning outputs may require multiple
output intersections; this basic version describes the assigned monitor only.

## Sampling and limits

The receiver queries Hyprland's local command socket (`j/activewindow` and
`j/monitors`) every 250 ms while subscribed. Polling catches geometry changes
even when focus stays on the same window. IPC runs in a separate task, has
bounded reply sizes/timeouts, and a latest-value watch channel prevents an
unbounded update backlog. No external `hyprctl` process is spawned per sample.

These are Hyprland's **layout bounds**. They are not guaranteed to match the
rendered rectangle during a scrolling animation. There is no animation-smooth
tracking, popup composition, screen-lock detection, screen capture, touch
coordinate injection, or video compression in this version. A future capture
path must attach frame/crop revisions and refuse stale taps rather than using
these samples as frame-accurate input coordinates.

## Checks

```sh
cargo test -p edge-protocol -p edge-crypto -p edge-receiver-linux
# Optional: read the current session without changing desktop state.
cargo test -p edge-receiver-linux live_hyprland_sample -- --ignored --nocapture
```

Tests cover encrypted subscription/permission enforcement, old-peer gating,
stop behaviour, MessagePack round trips, rotated/scaled outputs, negative
coordinates, offscreen windows, empty focus, IPC failure, and unchanged samples.
