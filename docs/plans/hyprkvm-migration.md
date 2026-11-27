# hyprKvm desktop migration

Status: consolidated implementation plan, 2026-11-16. Implementation started
2026-11-23 on `migration/hyprkvm`; see "Progress" below. No milestone has
passed its exit gate yet.
Baseline: `ba7a735` on main, including merged PR #26 and the revised
[UI design study](../mockups/hyprkvm-ui.html).

## Outcome and scope

Replace the egui desktop windows with Tauri + Svelte, align the desktop with
HyprMobile, and add focused-window View through the existing paired-computer
connection. Retain the backend, identities, trusted peers, portable operation,
tray use, audio, clipboard, notifications and input handover.

The first end-to-end target is **Tera as the Hyprland/NVENC host and the Windows
PC as viewer**. This is a chosen acceptance target, not a claim that no other
Linux computer exists. Linux Home must also work on Tera; Linux viewing gets a
separate decoder check and must not be advertised before it passes.

Initial View follows the focused window's visible region. Whole-monitor capture,
arbitrary/obscured-window capture, Windows hosting, a window picker and a second
View window are later work. Do not present these as functioning controls.
Audio retains its independent routing; synchronized audio/video is later work.

Use `migration/hyprkvm` when implementation starts. Keep egui usable until the
replacement passes acceptance on both operating systems. Split the work into
the reviewable units below; the migration branch is not one unreviewable final
commit. Documentation alone does not require creating that branch.

## Sources and corrections

This plan combines the risk-first order from the original Codex proposal with
Opus's layout, component and phase detail, incorporating Fable's code review:

- Fable comparison: <t3-thread://v2/475487c7-4369-4b4f-92f5-42eb2c18ae61/3691aa0b-6a0d-4695-9b7d-6d0c39fa8df3>
- Opus design and proposal: <t3-thread://v2/475487c7-4369-4b4f-92f5-42eb2c18ae61/8a05c085-f2dd-46ec-a52d-318966749bec>

Verified starting points:

- `crates/edge-ui` owns both widgets and backend-facing types (`NodeLink`, status,
  pairing input). Linux's `control.rs` / `control_link.rs` offer a small text
  socket API, not complete settings, permission editing or pairing decisions.
  Windows opens egui in-process and has no equivalent control endpoint.
- View capture, HEVC encoding and frame-scoped input live in Linux's
  `phone_remote_view.rs`, `phone_remote_video.rs` and `phone.rs`.
  `Frame::Phone` is explicitly phone-negotiated. PC sessions do not become
  viewers by adding a frontend button.
- Video availability currently requires wlr-screencopy and GStreamer's NVENC
  HEVC elements. GStreamer in the host does not establish a Windows decoder.
- HyprMobile's `hypr-core` depends on git-pinned `edge-*` crates from this repo.
  Importing it back here would create a repository dependency cycle and risk
  duplicate incompatible protocol types. Reusable code must move here first.
- HyprMobile has theme CSS, font dependencies and page-local markup, not an
  existing general card/tile/switch package.
- Phone grants are stored in `PinnedPhone`; PC pins currently hold only a
  fingerprint. Phone sessions cache some permissions. Live grants require
  backend changes, not direct pin-file edits from the UI.
- Installed identity/pins/logs are under `~/.local/state/edge-kvm`; config is
  under `~/.config/edge-kvm`. Portable paths are beside the backend executable.
  Existing releases package one backend binary per platform.

## Progress (2026-11-23)

Evidence: [`docs/testing/hyprkvm-m1-lua.md`](../testing/hyprkvm-m1-lua.md),
from Lua, not the target machines.

Done, Linux:

- `crates/edge-control`: UI-independent status DTOs and the settings draft
  and validation moved out of `edge-ui` (egui now uses them); versioned,
  bounded, length-prefixed JSON protocol with request IDs, typed errors,
  snapshots, events and lossless u64s; async client.
- Linux backend serves the protocol on the existing control socket beside
  the text commands (a framed client starts with a NUL byte). Status,
  capabilities, settings read/validate/save/restart with stale-draft
  refusal, sharing controls, controller hand-over, audio routing, phones,
  phone pairing with cancel-on-close, computer pairing arm, autostart,
  notifications, diagnostics and quit. Hello carries the instance's
  pid/config/state/log paths for instance matching.
- `apps/hyprkvm-ui` (Tauri 2 + SvelteKit static, own Cargo workspace):
  Home with every current settings-window section, Add a phone popup,
  View shown as unavailable, single instance with `--route`, backend
  absent/starting/incompatible/other-instance/disconnected states, offline
  settings editing, start backend via the user service or sibling binary.
- `packages/hypr-ui`: tokens, bundled OFL fonts, Card/Tile/Pill/Segment/
  Switch/Label/StatusDot. Desktop-owned (no HyprMobile snapshot yet).
- Tray opens hyprKvm only with `EDGE_KVM_DESKTOP_UI=hyprkvm`; egui stays
  the default.
- CI job `desktop-ui` (Linux and Windows): frozen npm install, checks,
  build, UI crate tests/Clippy, Tauri build. Backend jobs unchanged.

Not done:

- M0 on Tera and the Windows WebCodecs/HEVC experiment.
- M1 Windows control service (named pipe, current-user ACL) and the
  Windows backend's snapshot/settings handling. The Windows UI builds but
  can only report the backend as not running.
- M2 HyprMobile snapshot delivery; M3 Confirm computer popup and live
  grants; M4–M6.

## Architecture decisions

### Processes and local control

Use a separate `apps/hyprkvm-ui` Tauri process and the existing platform backend
process. The backend remains the sole owner of network sessions, capture,
identity/pins, configuration writes, permissions and control arbitration. It
continues running when the UI closes or crashes. Retain the backend tray during
the migration; it launches/focuses the single UI instance. Tauri owns its event
loop on its main thread, including pairing popups.

Extract UI-independent Rust request/event types into a proposed `edge-control`
crate. Both egui adapters and Tauri use it. Extend local control using a
versioned, bounded, length-prefixed protocol with request IDs, typed errors,
initial snapshots and subscribed changes. Retain existing text CLI commands
through an adapter during transition. Keep u64 frame/epoch IDs lossless across
the JS boundary (decimal strings).

Implement Unix sockets restricted to the current user on Linux and named pipes
restricted to the current user on Windows. Match the active backend instance
and its config/state paths; multiple portable folders must not silently attach
to a different instance. Treat endpoint version mismatches explicitly. Reconnect
refreshes state; it does not replay input, pairing approvals or start View.

Tauri commands expose explicit operations to bundled local pages. Do not expose
keys, arbitrary shell execution or writable pin files to JavaScript. Separate
bulk video delivery from the command/event queue; keep buffers bounded and
avoid base64. A UI crash must not strand capture or a pending approval.

### Desktop View transport and control

Use the existing encrypted PC connection and existing PC identity. Do not pair
the desktop a second time as a phone or import `hypr-core` into this repository.
Add a separately negotiated desktop-view extension and frame envelope, reusing
the existing bounded view payloads where suitable. Leave phone extensions and
wire messages compatible. Unsupported peers receive no unfamiliar events.

This is a substantial backend work package: protocol definitions/negotiation,
Windows session handling, both Linux session paths, permission storage, host
extraction, bounded scheduling, viewer reassembly and control ownership. Start
with chunked TCP and the existing Text/Media behaviour. Video must yield between
bounded chunks to input, heartbeats and other control traffic; congestion must
drop/recover video rather than trigger false disconnects. UDP is deferred unless
measurements demonstrate a need.

Extract platform-neutral frame/session assembly into this workspace; keep
Hyprland capture/injection behind a Linux host adapter. Reuse phone logic by
adapting the host, preserving all frame freshness, geometry and lock checks.
Phone control and desktop View consult the same backend control authority as
edge switching and role handover. Viewer UI state never grants a lease itself.

### Decode decision

First candidate on Windows: WebCodecs HEVC in Tauri's WebView2, based on
HyprMobile's existing fallback. Runtime-probe the exact codec/configuration and
measure the real compressed-byte IPC path. Do not assume HEVC or hardware decode
is available on every Windows installation.

If that fails on the target, investigate a Windows native decoder/surface
adapter before proceeding with View. Do not add GStreamer as a mandatory Windows
dependency by default. Linux video decoding is separately gated; native
GStreamer is a candidate there, not a decision justified by the host dependency.
JPEG can provide an explicitly labelled low-rate preview fallback; it does not
count as passing the Media-mode performance gate.

### Branding and compatibility

Use hyprKvm in visible branding and new UI executable/window identifiers. Retain
`edge-*` crate names, existing backend executable names and these operational
identifiers for this migration:

- Existing config and state paths, including portable layouts.
- `edge-kvm-receiver.service` and its installation behaviour.
- `EDGE_KVM_CONFIG`, `EDGE_KVM_STATE_DIR` and other existing environment overrides.
- Existing local socket/notification integrations and internal audio sink names
  such as `edge_kvm_remote` and `edge_kvm_phone`.

There is no on-disk directory migration. Repository renaming and removal of old
entry points are separate future work. The new UI discovers paths from the
backend; it must not create another identity in Tauri's default app-data folder.

## Milestones and exit checks

All tasks below are pending. Each milestone produces a short evidence record
under `docs/testing/`, listing tested commits, machines, commands, measurements,
failures and limitations. A successful build is not proof of hardware behaviour.

### M0 — feasibility before the full UI

- [ ] Run a minimal Tauri window on Tera under NVIDIA/Wayland: render the theme,
  resize between 279, 874 and 1250 px, reopen/focus from tray, open a popup and
  check suspend/resume. Record versions and any required renderer workaround.
- [ ] On the Windows PC, feed a representative captured HEVC sample through
  Rust → Tauri IPC → WebCodecs → canvas. Test Text and moving Media content,
  resize, keyframe reset and unsupported-codec behaviour. This sample is a
  decoder experiment, not a second phone pairing or the final transport.
- [ ] Record decoder support/hardware use, CPU, sustained fps, queue depth,
  memory and decode/display latency, with sample dimensions and a reproducible
  command. Proposed gate: sustain the delivered 10 fps Text / 45–55 fps Media
  samples for ten minutes without increasing queue or memory use; deliberately
  slow the renderer and demonstrate bounded frame dropping and recovery.

Exit: Tera Home rendering is viable and a Windows display route is measured.
If either fails, document and test a concrete alternative before committing to
the dependent UI/video work. No claim of zero-copy or 60 fps without evidence.
Linux View remains unavailable until its own decoder adapter passes this gate.

### M1 — backend contract and desktop shell

- [ ] Extract UI-independent types/validation; remove backend dependence on
  egui-owned status and notification DTOs without changing behaviour.
- [ ] Implement the shared control service and Linux/Windows transports.
  Cover status/capabilities, config read/validate/save/restart, sharing controls,
  devices/grants, pairing offers/decisions/cancel, diagnostics and autostart.
- [ ] Add `apps/hyprkvm-ui` using Tauri + SvelteKit static output; route tray and
  second-instance requests to Home, View or the appropriate pairing popup.
- [ ] Support backend absent, starting, incompatible and disconnected states.
  Preserve headless CLI/service use and standalone settings when disconnected.

Exit: both platforms show live backend status, apply a real supported action,
recover from a backend restart, and leave the backend healthy when UI exits.
Tests cover endpoint access, bounded/malformed messages, version mismatch,
config validation, instance selection and cancellation of pending operations.

### M2 — shared design assets and Home parity

- [ ] Create `packages/hypr-ui` here: theme tokens, bundled fonts and extracted
  Card, Tile, Pill, Segment, Switch, Label and StatusDot components. Keep device
  permissions, Tauri invokes and platform lifecycle out of these components.
- [ ] Define a versioned cross-repo delivery contract: HyprMobile vendors an
  immutable snapshot of this package at a recorded commit, imports it as a local
  package and commits the package lock. An export/update script verifies source
  commit, content checksum and font licences; no sibling-directory dependency.
  A registry publication can replace snapshots later.
- [ ] Make the first coordinated HyprMobile update an extraction with unchanged
  appearance/behaviour. Validate its build and Android smoke flows before
  marking the package shared. Until that update lands, call it desktop-owned.
- [ ] Build Home with status, arrangement, sharing, devices, View permissions,
  notifications, This computer and Advanced cards. Inventory every existing
  `settings.rs`, pairing and tray action in the acceptance checklist.
- [ ] Implement 279 px stacked rows / square tiles, 874 px two-column layout,
  and 1250 px expansion. No side navigation or decorative top bar. Keep Home
  and View as main-window routes, with Back/Esc/mouse-back/Alt+Left navigation.
- [ ] Sharing changes with backend live commands apply immediately and report
  committed state. Connection/layout/device settings that require restart use
  a validated draft and Discard/Save bar. Keep other unsupported live edits in
  that draft; do not restrict Save to the This computer card by assumption.

Exit: every current setting/action is mapped to a tested control or an explicit
platform-unavailable state. Verify audio routes/local playback/jitter,
clipboard text/images/phone forwarding, input/game compatibility, pairing,
notification enrollment/coordinator/bridge validation, autostart and log access.
Invalid saves leave running/persisted settings intact; restart preserves pins.
Keyboard navigation, focus indication, scaling and all three widths pass review.

### M3 — pairing popups and live permissions

- [ ] Implement separate Add phone (380×520) and Confirm computer (520×520)
  windows opened from Home, tray and incoming requests. Use `hyprkvm-pair`
  identifiers where supported, and ship a Hyprland float/centre rule snippet.
  Verify actual platform window identifiers rather than assuming window labels
  become Wayland app IDs. Preserve usable move/resize/close behaviour on Windows.
- [ ] Port code comparison, changed-key warning, fingerprint details, expiry,
  cancellation, simultaneous/replaced requests and successful-pair feedback.
  Both computers must still approve; closing/crashing a popup never approves.
- [ ] Add backend-owned live permission commands keyed by pinned fingerprint.
  Extend PC grant storage for View/Control/Focus with backwards-compatible
  defaults denying new capabilities; preserve existing phone grants.
- [ ] Serialize pin writes and notify active sessions. Revocation must stop
  affected capture/input and release held keys/buttons before acknowledging
  success. Audit cached phone grants, including focus and clipboard, so revoked
  permissions cannot remain usable through an existing session. Failed writes
  must not display a successful persistent grant; fail closed on revocation.
- [ ] Show local grants separately from what the peer permits. A remote grant
  cannot be edited locally; report that it must be changed on the host.

Exit: popup state-machine tests cover timeout/cancel/changed-key acceptance.
Integration tests change grants during connected phone/PC sessions, reject
queued stale actions, preserve unrelated permissions and verify persistence
after reconnect. No manual edits while the receiver is stopped are needed.

### M4 — View over the PC connection, view-only

- [ ] Land protocol/capability negotiation and old/new peer compatibility tests.
  Add PC view messages without sending them to unnegotiated PCs or phones.
- [ ] Extract the host service, add the viewer assembler and wire Windows and
  both Linux PC-session paths. Separate view lifetime from control roles/audio.
- [ ] Connect the proven decoder with displayed-frame metadata, bounded
  reassembly/decode queues, frame acknowledgements and keyframe recovery.
  Configure Text/Media, Fit/Fill/1:1, pan and stream diagnostics.
- [ ] Implement connecting, live, locked, denied, unsupported, stalled and
  disconnected states. Stop on View exit, UI loss, peer lock or grant removal;
  reopening/reconnecting requires an explicit View request. Audio remains
  governed by its own user-selected route.

Exit: Windows views Tera end to end in Text and Media. Resize/window/focus
changes recover correctly; slow viewing and a congested link do not starve KVM
input, clipboard, audio, heartbeats or role handover. Test malformed chunks,
oversized frames, missing acks and decoder reset. Re-run M0 measurements on the
real network path. Windows hosting is explicitly unsupported.

### M5 — View interaction and common control ownership

- [ ] Start in Adjust; it sends no remote input. Interact requires the host's
  grant and a backend lease. Map clicks, right-click, wheel, drag and keyboard
  to the displayed frame/crop; retain stale-frame and geometry guards.
- [ ] Integrate View, phone and edge-switching control into one arbitration
  policy. Edge activation or role handover preempts View, releases held input,
  and returns its UI to Adjust. View never silently steals control back.
- [ ] Use Right Ctrl as the proposed configurable local release key, subject
  to Windows/Wayland tests; consume it locally. Keep Ctrl+Alt+Pause as the
  existing emergency path. Do not use Ctrl+Alt as the View default (GB AltGr).
  Add explicit Super-arrow controls because compositor bindings may intercept
  Super; test which shortcuts the platform can actually capture.
- [ ] Release all held keys/buttons on blur, navigation, disconnect, lock,
  revocation, timeout and UI crash. Distinguish typing focus from navigation:
  release keyboard ownership before routing Back/Esc away from View.

Exit: real two-machine tests cover GB layout/AltGr, modifiers, release key,
focus loss mid-drag, stale geometry, network loss, simultaneous phone control,
edge crossing and role changes. There is always a working local escape path.

### M6 — packaging, cutover and compatibility

- [ ] From M1, run frozen Node install, Svelte/type checks, frontend build,
  Rust tests/Clippy and Tauri builds on Windows and Linux CI. Keep backend-only
  jobs independent of UI system dependencies. Pin toolchains/lockfiles.
- [ ] Add Linux WebKitGTK/Tauri build/runtime dependencies and verify Tera's
  actual distribution. Document supported distro/runtime versions. On Windows
  detect WebView2 and give an actionable setup path when absent; separately
  report unavailable HEVC decode. Do not assume a portable ZIP bundles a webview.
- [ ] Package UI and platform backend together with fonts, licences, README
  and checksums. Retain backend names and installed paths; document updating
  both executables. Test service-backed and portable layouts, including spaces
  in paths and launching the UI from outside its directory.
- [ ] Ensure normal launch starts/attaches to exactly one backend; never start
  a duplicate alongside the installed service. Explicit Quit stops the intended
  processes; closing Home only closes the UI. UI failure retains CLI recovery.
- [ ] Switch tray defaults to Tauri only after M2/M3 parity. Keep an egui
  fallback until final acceptance; remove it and its dependencies in a separate
  final change after View and packaging checks pass.

Exit: clean-machine Windows/Linux installs, upgrades of current portable and
service installations, and rollback all preserve identity/pins/config. Existing
HyprMobile revision still pairs/connects and uses clipboard, audio, notifications
and View. Test any coordinated mobile rev bump with every `edge-*` dependency
pinned to the same revision; unchanged wire contracts need no automatic bump.
Tagged releases publish only after both platform jobs pass; PRs do not publish.

## Work order and relative scope

Dependency order: M0 → M1 → M2/M3 → M4 → M5 → M6 cutover. CI/package work from M6
starts in M1; it is not postponed until the last milestone. Design extraction
does not block M0. Keep these review units explicit:

1. Feasibility harnesses and measured decisions (unknown effort until run).
2. Shared control types plus Linux transport; Windows service/transport and
   lifecycle (substantial new work, not a thin NodeLink wrapper).
3. Shared assets/component delivery and Home parity (medium/large UI work).
4. Pairing lifecycle and live grants (substantial state/persistence work).
5. PC View protocol, host extraction/scheduling, viewer/decoder integration
   (three substantial work units spanning both platform session loops).
6. Interaction/arbitration, package acceptance, then egui removal.

These are relative sizes, not elapsed-time estimates. Re-estimate after M0 and
the control/protocol interface designs; do not describe the whole migration as
frontend-only or promise unchanged core behaviour without regression checks.

## Review coverage and remaining evidence

- Fable's machine/decoder gap: first target and Windows-specific M0 gate.
- Tera renderer gap: independent Wayland/NVIDIA Home gate before UI work.
- Live permissions gap: M3 persistence, session-cache invalidation and revocation.
- Packaging gap: two executables, Node CI, webview/runtime checks and upgrades.
- Deferred choices gap: separate UI/backend processes, PC transport and the
  gated decoder candidate are selected above.
- Protocol-cost gap: explicit M4 work units, scheduling and compatibility tests.
- Thin phase checks gap: each milestone has concrete acceptance evidence.
- Reverse dependency / second pairing gap: no `hypr-core` import or phone-type
  desktop identity; existing PC trust is retained.
- Linux-only socket gap: Windows IPC and config/approval API are new M1 work.
- Rename gap: preserve state/config/service/env/audio identifiers entirely.
- Shared-package gap: create components and deliver immutable snapshots.
- Linux-only decoder gap: no mandatory Windows GStreamer dependency.

Still unmeasured: target WebView2 HEVC support/performance, Tera Tauri rendering,
Linux viewer decoding, and release-key interception on each desktop. M0/M5 own
these gates. Completing this planning document does not mark any of them passed.
