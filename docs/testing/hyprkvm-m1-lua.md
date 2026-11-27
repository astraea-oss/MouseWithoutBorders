# hyprKvm M0/M1 evidence: Lua (Linux)

Date: 2026-11-23. Branch `migration/hyprkvm` (uncommitted working tree at the
time of testing; record the commit when this lands).

This is **not** the plan's acceptance target. The plan's M0 Linux gate is
Tera (Hyprland/NVENC host) and its Windows gate is the Windows PC; neither
was available. Lua is a second Linux machine, so these results show the
software works but don't pass M0 or M1.

## Machine

| | |
| --- | --- |
| Host | Lua, CachyOS, Hyprland 0.57.2 (Lua config), Wayland |
| GPU | NVIDIA GeForce RTX 2050, driver 620.70.06 |
| WebKitGTK | 2.54.2 (webkit2gtk-4.1) |
| Tauri | 2.12.7 (crate), CLI 2.12.6 |
| Frontend | Svelte 5.59.1, SvelteKit 3.4.0, Vite 8.3.8 |
| Rust | 1.96.0 |

## Method

The installed `edge-kvm-receiver.service` was left running and untouched.
A second backend built from this branch ran isolated: its own
`XDG_RUNTIME_DIR`, `EDGE_KVM_STATE_DIR`, `EDGE_KVM_CONFIG` and
`XDG_CONFIG_HOME`, listening on `127.0.0.1:42499`. The UI ran with the same
environment on a temporary headless Hyprland output, so nothing opened on
the user's screen. Windows were sized with Hyprland dispatchers and
captured with `grim`. A small Python client spoke the framed protocol
directly for the save/restart and quit checks.

## Results

| Check | Result |
| --- | --- |
| Tauri window renders on NVIDIA/Wayland, no renderer workaround needed | Pass |
| Bundled Doto / Space Grotesk / Space Mono load (no network fonts) | Pass |
| Window class is `hyprkvm` (`hyprctl clients`) | Pass |
| Home at 279 × 1428 (stacked, 2 × 2 tiles) | Pass after fixing long audio labels |
| Home at 874 × 1428 (two columns) | Pass |
| Home at 1250 × 1428 (two columns, four tiles in a row) | Pass after tile change |
| Against the old installed backend: reported as "Update", no hang | Pass |
| Live status from the new backend (name, address, summary, version) | Pass |
| Old text CLI (`list-phones`) still works on the shared socket | Pass |
| Second launch `--route pair-phone` hands over to the running instance and exits | Pass |
| Add a phone popup: 380 × 520, real QR code, countdown | Pass |
| Closing the popup cancels the pairing in the backend (`phone pairing cancelled` in the log) | Pass |
| Save settings over the protocol: validate, write, restart, same identity fingerprint | Pass |
| UI reconnects after the restart and shows the saved name | Pass |
| Arm computer pairing → UI hero updates to "Pairing" from a pushed snapshot | Pass |
| Live action while disconnected returns a typed `unavailable` error | Pass |
| UI killed with SIGKILL → backend still serves commands | Pass |
| Backend quit → UI shows Offline with Start backend, settings still editable | Pass |

Automated tests (all pass): `edge-control` framing, protocol and draft
tests; `edge-receiver-linux` `control_v1` tests for hello/instance identity,
incompatible versions, malformed and oversized frames, refusal without a
tray, invalid and stale saves leaving the file untouched, validation without
writing, and phone pairing outcome plus cancellation on disconnect.

## Not tested / limitations

- Suspend/resume and reopening from the tray were not exercised.
- Clicks inside the UI were not automated; actions were exercised through
  the same backend requests the UI's commands send.
- Windows: no Windows machine. The Windows backend has no control endpoint,
  so the Windows UI can only show "not running" today.
- WebCodecs/HEVC decode (the M0 Windows gate) was not attempted.
- Tera itself (NVENC host) was not available.

## Re-test against the live link (2026-11-23, after `142d4d9`)

Installed service backend on Lua, linked to Tera over the LAN. Driven through
the control protocol and the UI on a headless output.

| Check | Result |
| --- | --- |
| Snapshot: Linked, peer Tera, round trip in ms, Tera controlling | Pass |
| Start at login reports the installed service as enabled (fix in `142d4d9`) | Pass |
| Peer address validation: with port, bare IPv6, invalid name rejected; IPv4, `[IPv6]`, host name accepted; config untouched | Pass |
| Input forwarding off/on: applied, pushed snapshot within the same tick | Pass |
| Disconnect → Paused within 0.1 s; Reconnect → link and audio back | Pass |
| Add a phone popup floats and centres at 380 × 520 without a window rule (fix in `142d4d9`) | Pass |
| Closing the popup cancels the pairing | Pass |
| Hero address under the peer's name | **Failed, fixed**: it showed this computer's VPN address (`tun0`). Now the peer's address when linked, and this computer's address on the route to the peer otherwise |

Seen, not changed: on each backend start Lua's own dial to Tera is refused
with "pairing needs approval on both computers" while Tera's dial to Lua
succeeds a second later. It predates this branch's builds.
