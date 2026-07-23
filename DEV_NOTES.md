# Development Notes

## Linux Input Backend

This project is currently targeting Lua's CachyOS/Hyprland laptop first.

On this machine, Arch/CachyOS packages `libei` with this pkg-config module:

```bash
pkg-config --modversion libei-1.0
```

Do not probe `libei`; that name is not provided here. The installed package is:

```text
extra/libei 1.6.0-1
```

Current behavior:

- `input.backend = "auto"` detects `libei-1.0`.
- The libei sender path is wired through `liboeffis-1.0`, but this laptop's
  current Hyprland portal does not expose `org.freedesktop.portal.RemoteDesktop`
  and there is no direct EIS socket, so libei initialization fails here.
- After libei fails, `auto` falls back to the Hyprland/wlroots virtual input
  backend.
- If neither real backend initializes, `auto` exits with an error. This lets a
  service manager retry after the graphical-session environment becomes
  available and prevents a connected receiver from silently discarding input.
- `input.backend = "log"` explicitly enables protocol-only testing without
  local input injection.
- `input.backend = "hyprland"` forces the Hyprland virtual input backend and is
  the correct development mode for Lua's current setup.
- `input.backend = "libei"` is strict and should fail clearly if the portal/EIS
  path is unavailable.

Verified local commands:

```bash
EDGE_KVM_CONFIG=/tmp/edge-kvm-hyprland-test.toml \
EDGE_KVM_STATE_DIR=/tmp/edge-kvm-hyprland-test-state \
cargo run -p edge-receiver-linux -- --test-input pointer

EDGE_KVM_CONFIG=/tmp/edge-kvm-hyprland-test.toml \
EDGE_KVM_STATE_DIR=/tmp/edge-kvm-hyprland-test-state \
cargo run -p edge-receiver-linux -- --test-input click

EDGE_KVM_CONFIG=/tmp/edge-kvm-hyprland-test.toml \
EDGE_KVM_STATE_DIR=/tmp/edge-kvm-hyprland-test-state \
cargo run -p edge-receiver-linux -- --test-input key
```

All three initialize the Hyprland Wayland virtual input backend and exit 0.

## Linux Tray

The Linux receiver publishes a KDE/freedesktop StatusNotifierItem by default,
which Waybar's tray module can display. The tray shows:

- current receiver state;
- listen address;
- active input backend;
- connected peer;
- connection, input, and clipboard counters;
- last receiver error when one occurs.

The tray menu includes a `Quit receiver` action. Use `--no-tray` for diagnostic
or headless runs:

```bash
cargo run -p edge-receiver-linux -- --pair --no-tray
```

Verified on Lua's Waybar session:

```text
org.kde.StatusNotifierWatcher                  waybar
org.kde.StatusNotifierItem-<pid>-1             edge-receiver-l
Title: edge-kvm receiver: Listening
```

Later portability work:

- The portal-based libei sender is now opt-in because several distributions do
  not package `liboeffis`. Build it with
  `cargo build -p edge-receiver-linux --features libei` when both
  `libei-1.0` and `liboeffis-1.0` are available. The default portable build
  keeps the Hyprland/Wayland backend and has no libei link requirement.
- Probe multiple pkg-config names if needed, starting with `libei-1.0`.
- Replace the manual libei/liboeffis FFI with generated or maintained bindings.
- Re-test libei when Hyprland/xdg-desktop-portal-hyprland exposes
  RemoteDesktop/ConnectToEIS or another EIS socket path.
- Keep a strict `libei` mode that fails if real injection cannot be initialized.
- Keep log-only mode explicit so production receivers cannot appear healthy
  while discarding input.
