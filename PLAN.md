# edge-kvm Project Plan

Last reviewed: 2026-10-27 against `main` at `33efae2`.

## Purpose and current scope

Build a portable Rust LAN software KVM that shares mouse, keyboard, clipboard,
audio, and selected device integrations. Either computer can control the other;
input direction is independent of audio direction and who opened the connection.

The implemented baseline is a pair of computers: Windows/Linux or Linux/Linux.
Linux nodes also support multiple paired phones. A fleet of several computers is
planned, not implemented. This plan replaces the original one-way
Windows-to-Hyprland MVP plan.

## Implemented on main

- **Input:** Windows and Linux capture/injection, edge switching, runtime control
  handover, forwarding toggles, emergency release, liveness checks, and held-input
  cleanup. Linux capture uses the InputCapture portal; injection supports uinput
  on Niri and the available libei/Hyprland paths.
- **Linux nodes:** every Linux computer accepts computer and phone connections
  and can dial its configured computer. Simultaneous dialing is deduplicated using
  device fingerprints. Settings exposes an optional other-computer address rather
  than a connect/wait role choice. Windows still initiates connections only.
- **Pairing and protocol:** persistent identities, pinned peer fingerprints,
  encrypted sessions, and two-sided pairing confirmation. Protocol v2 includes
  role-neutral configuration migration with adjacent v1 backups; both computers
  must run the same protocol generation.
- **Clipboard:** bidirectional text and bounded static images, loop prevention,
  and chunked large text with capability negotiation. Arbitrary file transfer is
  outside the current scope.
- **Audio:** routes in either direction independently of input control, Windows
  WASAPI capture, Linux capture, and clock-paced Linux playback with drift
  correction, buffering, diagnostics, and ALSA fallback.
- **Desktop controls:** one-page settings, tray menus, named control/audio choices,
  connection status, and Linux service installation while retaining portable use.
- **Phones on Linux:** multiple paired phones, QR pairing, clipboard,
  notifications, and audio. This repository's PC implementation does not imply
  feature parity on Windows or every capability in the separate phone app.
- **Shared Linux notifications:** opt-in Quickshell integration, one configured
  history owner, source labels, durable state and queued operations, offline
  reconciliation, synchronized dismissals, and phone ingestion into shared history.
  Setup uses the existing settings window and supported notification centres.

Recent merged milestones:

- [#20: One-page settings and phone pairing](https://github.com/astraea-oss/MouseWithoutBorders/pull/20)
- [#22: Universal Linux nodes](https://github.com/astraea-oss/MouseWithoutBorders/pull/22)
- [#24: Shared Linux notifications](https://github.com/astraea-oss/MouseWithoutBorders/pull/24)
- [#19: Input latency, clock-driven audio, and large clipboard text](https://github.com/astraea-oss/MouseWithoutBorders/pull/19)

## Verification status

The [latest main CI run](https://github.com/astraea-oss/MouseWithoutBorders/actions/runs/40053194426)
on 2026-10-24 passed workspace tests, lint, and release builds on Windows and
Linux for `33efae2`. No pull requests were open when this plan was reviewed.
These are historical CI results, not a new physical-device acceptance run.

Live development testing has exercised Linux/Linux capture, handover, emergency
release, reconnect, and audio. The complete acceptance matrix still needs a
recorded run, especially both Windows/Linux control directions and shared
notifications on the physical Linux PCs. Implementation and automated test
coverage should not be treated as completed hardware acceptance.

## Next milestones

### 1. Validate the merged two-computer baseline

Deploy matching builds to the physical computers while preserving existing
configuration, device identities, and pairings. Use the
[Windows/Linux baseline](docs/testing/windows-linux-baseline.md) and
[Quickshell setup and limits](integrations/quickshell/README.md).

- Verify Windows/Linux control in both directions and Linux/Linux control,
  handover, forwarding toggles, and all configured screen edges.
- Check held keys/buttons, the emergency release shortcut, process failure,
  network loss, reconnect, and clean shutdown without trapped input.
- Exercise text, Unicode, images, oversized clipboard content, and audio in both
  directions, including route changes and a sustained playback session.
- Check Linux simultaneous dialing settles on one session, and phone features
  remain available regardless of which Linux computer opened the PC connection.
- On both Linux PCs, verify notification setup, native and phone entries,
  dismiss/Clear, offline recovery, shell reload/restart, and daemon restart.
- Record commit, OS/compositor, display geometry/scales, network, test results,
  and remaining defects. Fix regressions before expanding topology.

Completion means the recorded checks pass or remaining limitations are explicitly
accepted and documented. This milestone is still pending.

### 2. Finish neutral naming and migration

Continue [issue #21](https://github.com/astraea-oss/MouseWithoutBorders/issues/21).
Its Linux node loop and simplified address setting have landed; naming and fleet
support remain follow-up work.

- Introduce role-neutral `edge-kvm` / `edge-kvm.exe` entry points,
  `edge-kvm.service`, and neutral configuration/log names.
- Keep legacy executable names as aliases for one release and document the
  deprecation window. Migrate existing service installations without enabling
  duplicate daemons or discarding configuration, identity, pins, or state.
- Verify fresh portable use, existing portable upgrades, service upgrades, and
  compatibility aliases. Update installation instructions and diagnostics.
- Reconcile older design documents and examples with the implemented node model;
  some still describe the earlier fixed connector/listener architecture.

Completion means new and existing installations work with a documented migration
path and unchanged trust relationships.

### 3. Add multi-computer fleet support

Keep this separate from naming changes. Start with an explicit fleet design and
migration plan before extending the working two-computer runtime.

- Add multiple configured computer peers, per-peer pairing/session state, and
  per-peer screen-edge layout, preserving existing single-peer configuration.
- Define control ownership and handover across several computers, with one input
  destination at a time and release behavior for destination failure.
- Route edge crossings to the intended peer and keep connection deduplication
  stable when several nodes dial concurrently.
- Specify clipboard propagation and loop prevention across the fleet, and define
  audio routing alongside concurrent PC and phone sessions.
- Extend settings/status for several peers without exposing transport plumbing.
- Add meaningful multi-peer tests and validate at least three physical computers
  before claiming fleet support.

Completion means a documented, tested three-computer workflow with safe recovery
and no regression to existing pairs. LAN discovery and Internet relays remain
outside this milestone.

### 4. Extend integrations after the baseline is stable

These are follow-up candidates rather than committed prerequisites for fleet work:

- Windows phone support and native Windows notification integration.
- PC-to-Android dismissal, requiring a corresponding mobile protocol command.
- Notification incremental replay and safe terminal-marker compaction.
- Coordinator migration/reset and clearer history maintenance/recovery tooling.
- Remote notification popups, actions/replies, and richer image payloads.

Keep each extension separately scoped with protocol compatibility, recovery, and
acceptance criteria agreed before implementation.

## Current limits and safety requirements

- The shipped PC topology remains two computers; multiple phones are supported
  on Linux. Windows is not yet a symmetric listening node.
- Windows injection does not bypass elevated-process restrictions or support the
  UAC secure desktop. macOS, automatic LAN discovery, and Internet relays are not
  part of this plan.
- Shared notifications initially require supported Linux Quickshell centres and
  one explicitly configured owner. Remote copies are silent; DND stays local.
- PC dismissals synchronize the PC copies of phone notifications; they do not
  dismiss the original Android notification with the current phone protocol.
- Notification storage/queues are bounded. Automatic terminal-marker compaction,
  leader election, and coordinator migration/reset are not implemented. Follow
  the integration guide for capacity and recovery limits.
- Preserve pinned trust, bounded payloads/queues, clipboard loop prevention,
  emergency release, and held-input cleanup through every milestone. Notification
  or audio failures must not trap input or take down unrelated features.

## Validation and supporting documents

For runtime changes, use the platform-specific test, lint, and release-build
commands in [the baseline checklist](docs/testing/windows-linux-baseline.md).
For notification changes, also run the focused tests and optional offscreen
Quickshell bridge test in [the integration guide](integrations/quickshell/README.md).
Record physical-device acceptance separately from automated checks.

- [README: current operation and configuration](README.md)
- [Universal control design and implementation notes](docs/plans/universal-controller-receiver.md)
- [Shared notifications design and deferred work](docs/plans/shared-notifications.md)
- [Linux audio streaming](docs/plans/linux-audio-streaming.md)
- [Bidirectional clipboard](docs/plans/bidirectional-clipboard.md)
- [Clipboard images](docs/plans/clipboard-images.md)

The linked design plans retain historical architecture and target behavior.
Use their implementation-status notes, the current README, and the integration
guide to distinguish shipped behavior from proposals.
