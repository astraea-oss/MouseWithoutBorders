# Windows to Linux Baseline

Run this checklist before and after each phase that touches protocol, runtime,
input, clipboard, audio, configuration, or packaging. Both peers must be built
from the same protocol generation; protocol v1 and v2 intentionally do not
interoperate.

## Automated checks

From the repository root:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

If the host cannot build a platform-specific package, run the workspace checks
on both Windows and Linux and record the two results in the pull request.

## Live setup

- Use the existing real controller and receiver configuration files, copied to a
  temporary portable test directory beside the matching binaries.
- Start the Linux receiver, then the Windows controller.
- Confirm the displayed device names and Noise fingerprints match the intended
  pair before accepting a new trust prompt.
- Record Windows and Linux versions, desktop/compositor, display resolutions and
  scales, network type, commit, and whether the pair was already trusted.

## Required behavior

- Pairing: a fresh pair requires consent; a trusted pair reconnects without a
  new prompt; a fingerprint mismatch fails closed.
- Pointer: cross the configured Windows edge at the top, middle, and bottom;
  motion and landing position remain proportional on the Linux output.
- Return: leave through Linux's return edge at the top, middle, and bottom; the
  cursor returns to the corresponding Windows position.
- Buttons and wheel: left, right, middle, back, forward, vertical wheel, and
  horizontal wheel neither duplicate nor stick.
- Keyboard: normal text, held Shift/Ctrl/Alt/Super, repeats, and shortcuts arrive
  once; the emergency release hotkey remains local.
- Forwarding toggle: disabling forwarding releases remote input immediately and
  entering the remote edge cannot reactivate it until explicitly enabled.
- Clipboard: text moves in both directions, including multiline Unicode; images
  move in both directions up to the configured bound; oversized content fails
  without dropping the session.
- Audio: when enabled and supported, Windows capture reaches Linux playback;
  disable/re-enable and disconnect cleanly stop the stream. When disabled, no
  audio device or UDP stream is opened.
- Reconnect: tray Disconnect stops the active connection; Reconnect starts it;
  ordinary network loss backs off instead of hot-looping.
- Recovery: killing either process, removing the network, and stopping a peer
  while keys/buttons are held restores local control and releases remote input.
- Shutdown: both applications exit without leaving virtual devices, held input,
  audio routing, consent windows, or helper processes behind.

## Phase 3a edge accuracy

The protocol-v2 flag day changes the edge-position field. Before adding Linux
capture, repeat entry and return checks for left, right, top, and bottom placement
using displays with different resolutions and scales. Compare the observed
normalized landing positions with the shared-geometry test fixtures.
