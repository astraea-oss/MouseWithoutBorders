# Bidirectional Clipboard Synchronization

## Summary

Make text clipboard synchronization work automatically in both directions:

- Windows main PC to Linux secondary laptop.
- Linux secondary laptop to Windows main PC.

The existing encrypted control session and text-only clipboard packet format remain in use. Clipboard synchronization must not affect keyboard, mouse, audio, heartbeat, or reconnect behavior.

## Current Gap

Windows-to-Linux paste already sends the Windows clipboard before forwarding `Ctrl+V` to Linux. Linux-to-Windows transfer currently depends on a narrower fallback: while Windows is controlling Linux, a forwarded `Ctrl+C` schedules a clipboard request after 200 ms.

That fallback does not observe copies made directly with the laptop keyboard, mouse, application menus, or clipboard tools. It also means returning to Windows and pressing paste does not itself request the Linux clipboard.

## Selected Behavior

- Observe text clipboard changes on both devices while they are connected.
- Offer changed text immediately over the authenticated Noise/TCP session.
- Retain the existing `Ctrl+V` pre-send and `Ctrl+C` request paths as low-latency and compatibility fallbacks.
- Synchronize text only and enforce the existing `clipboard.max_bytes` limit.
- Ignore non-text clipboard formats rather than replacing them.
- Keep clipboard errors isolated from the KVM and audio session.
- Preserve all portable storage rules; no clipboard data is written to disk.

## Change Detection

### Windows

Poll the local text clipboard at a short bounded interval while connected. Reading the Win32 clipboard is local and inexpensive, but a temporarily locked clipboard must only skip that poll rather than disconnect the session.

### Linux

Use the already-required `wl-paste` utility in event-driven `--watch` mode. The watcher emits a notification when the Wayland clipboard changes; the receiver then reads the current text with the existing bounded `wl-paste` path.

The watcher must terminate with its session and retry if `wl-paste --watch` exits unexpectedly. This adds no library, daemon, installer, or CMake dependency.

## Loop and Conflict Prevention

Maintain an in-memory tracker on each device containing:

- The last observed local text value.
- A monotonically increasing local offer sequence.

Rules:

1. Do not send an offer when the observed value is unchanged.
2. After applying remote text locally, mark it observed before processing the resulting local clipboard notification.
3. If a remote offer arrives while an unobserved local change exists, keep and offer the local value instead of overwriting it.
4. A cleared or non-text clipboard resets the observation without transmitting an empty replacement.
5. Never persist clipboard text or log its contents.

## Implementation Checklist

- [x] Add a shared, testable clipboard change tracker.
- [x] Add connected-session Windows clipboard change polling.
- [x] Add an event-driven Linux Wayland clipboard watcher.
- [x] Send unsolicited Linux text offers to Windows.
- [x] Suppress remote-write echo loops in both directions.
- [x] Keep existing copy/paste-triggered fallback behavior.
- [x] Rate-limit or suppress transient clipboard access errors.
- [x] Add tracker and platform integration tests.
- [x] Build portable Windows and Linux release artifacts in CI.

## Acceptance Criteria

- Copying text on Windows makes it immediately pasteable on Linux.
- Copying text on Linux makes it immediately pasteable on Windows.
- Copies made with application menus or the laptop's physical input devices synchronize.
- Repeated identical clipboard notifications do not produce a network loop.
- Multiline text, Unicode, and text near the configured size limit round-trip correctly.
- Oversize or non-text clipboard content does not terminate the connection.
- Simultaneous changes resolve without endlessly bouncing values.
- Clipboard synchronization resumes after reconnecting either application.
- Input and audio behavior remain unchanged under clipboard activity.
- No app-owned clipboard state is written outside the portable executable directories.

## Validation

- Unit-test change detection, sequencing, remote-apply suppression, clearing, and conflict handling.
- Run the Windows workspace tests and release build.
- Run the Linux workspace tests and release build in CI.
- Perform live copy/paste checks in both directions using keyboard shortcuts and application menus.

## Live Acceptance Results

- [x] Windows-to-Linux text copy and paste works on the physical devices.
- [x] Linux-to-Windows text copy and paste works on the physical devices.
- [x] Clipboard activity produces no synchronization errors or repeated network loop.
- [x] Input remains operational during clipboard synchronization.
- [x] Audio remains streaming with stable queue depth after clipboard testing.
- [x] Portable configuration and state remain beside the executables.
