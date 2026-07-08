# Windows-to-Hyprland LAN KVM Plan

## Summary

Build a focused personal software KVM from scratch in Rust:

- **Controller:** Windows 11 PC with the physical keyboard/mouse.
- **Target:** this CachyOS/Hyprland laptop on Wayland.
- **Direction:** Windows controls the laptop; the laptop does not capture physical input.
- **Input backend:** Linux uses `libei` first for Wayland input emulation.
- **Clipboard:** bidirectional **text-only** clipboard sync in the first usable version.
- **Security:** pinned-key encrypted pairing.
- **Safety:** global Windows release hotkey, timeout release, and stuck-key cleanup.

This deliberately avoids a general cross-platform product. The first goal is "my Windows PC controls my Hyprland laptop reliably."

## Confirmed Environment

- Laptop OS: CachyOS, Arch-like.
- Laptop session: Hyprland 0.55.4 on Wayland.
- Laptop monitor: `eDP-1`, `1920x1080`, scale `1.0`, 144 Hz.
- Rust installed: `rustc 1.96.0`, `cargo 1.96.0`.
- Linux libraries available: `libei 1.6.0`, `libevdev`, `wayland-client`, `xkbcommon`, `gtk4`, `libadwaita`.
- Clipboard tools available: `wl-copy`, `wl-paste`.
- `/dev/uinput` is root-only, so it is not the first backend.

## Project Layout

Create a Rust workspace at:

```text
/home/lua/Desktop/edge-kvm
```

Workspace crates:

```text
edge-kvm/
  Cargo.toml
  crates/
    edge-common/
    edge-protocol/
    edge-crypto/
    edge-geometry/
    edge-keymap/
    edge-linux-input/
    edge-windows-input/
  apps/
    edge-controller-win/
    edge-receiver-linux/
```

Responsibilities:

- `edge-common`: shared config structs, errors, logging setup.
- `edge-protocol`: network frame types and serialization.
- `edge-crypto`: pinned identity keys and encrypted session setup.
- `edge-geometry`: screen layout, edge transition, coordinate mapping.
- `edge-keymap`: Windows scancode to Linux evdev keycode mapping.
- `edge-linux-input`: Linux `libei` input injection and clipboard integration.
- `edge-windows-input`: Windows Raw Input, hooks, clipboard listener, tray helpers.
- `edge-controller-win`: Windows tray app/controller.
- `edge-receiver-linux`: Linux receiver daemon.

## External APIs And Libraries

Use Rust dependencies:

```text
tokio
serde
toml
rmp-serde
bytes
thiserror
tracing
tracing-subscriber
snow
ed25519-dalek or x25519-dalek as needed by Noise setup
windows
tray-icon
arboard, Windows clipboard APIs, or direct Win32 clipboard APIs
bindgen
pkg-config
```

Linux input:

- First backend: `libei` through generated FFI bindings using `bindgen` and `pkg-config`.
- Fallback 1 if `libei` fails on Hyprland: wlroots virtual pointer/keyboard protocols.
- Fallback 2 if compositor protocols fail: `uinput` with a udev rule, not root runtime.

References used for design constraints:

- Microsoft `SetWindowsHookEx`: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowshookexa
- Microsoft `SendInput`: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput
- libei docs: https://libinput.pages.freedesktop.org/libei/

## Runtime Model

Windows has two control states:

```text
LocalActive
RemoteActive(peer = laptop)
```

`LocalActive`:

- Windows mouse and keyboard behave normally.
- App watches cursor position.
- If cursor exits the configured edge, switch to `RemoteActive`.

`RemoteActive`:

- Windows captures physical mouse deltas using Raw Input.
- Windows suppresses local keyboard/mouse delivery using low-level hooks.
- Windows sends input events to Linux over the encrypted LAN session.
- Windows maintains a virtual laptop cursor position.
- If virtual cursor exits the opposite edge, return to `LocalActive`.

This avoids the Lan Mouse-style trap where the laptop also captures and has to return control. The laptop only receives and injects.

Default layout:

```text
Laptop is left of Windows.
Windows exits left edge -> laptop.
Laptop exits right edge -> Windows.
```

## Network Protocol

Use a single persistent TCP connection from Windows to Linux.

Default port:

```text
42420/tcp
```

Serialization:

- Length-prefixed MessagePack using `rmp-serde`.
- Each frame: `u32_be length` + encoded payload.

Transport security:

- Use `snow` with a Noise XX-style handshake.
- Each device has a persistent identity key.
- First pairing pins the peer public key.
- Later connections reject changed peer keys unless explicitly re-paired.

Protocol frames:

```rust
enum Frame {
    Hello(Hello),
    ScreenInfo(ScreenInfo),
    Input(InputEvent),
    Clipboard(ClipboardEvent),
    Control(ControlEvent),
    Heartbeat(Heartbeat),
    Error(RemoteError),
}
```

Core types:

```rust
struct Hello {
    protocol_version: u16,
    device_name: String,
    role: Role,
    public_key_fingerprint: String,
}

struct ScreenInfo {
    outputs: Vec<OutputInfo>,
    primary_output: String,
}

struct OutputInfo {
    name: String,
    width: u32,
    height: u32,
    scale: f32,
    x: i32,
    y: i32,
}

enum InputEvent {
    PointerMotion { dx: f64, dy: f64 },
    PointerButton { button: MouseButton, down: bool },
    PointerWheel { x: f64, y: f64 },
    Key { evdev_code: u16, down: bool },
    AllKeysUp,
}

enum ClipboardEvent {
    TextOffer { sequence: u64, text: String },
    TextRequest,
}

enum ControlEvent {
    EnterRemote { edge: Edge, normalized_y: f32 },
    LeaveRemote { edge: Edge, normalized_y: f32 },
    ReleaseToLocal { reason: ReleaseReason },
}
```

## Config Files

Windows config:

```toml
device_name = "Main PC"
role = "controller"
release_hotkey = "Ctrl+Alt+Pause"

[peer.laptop]
host = "192.168.0.11"
port = 42420
position = "left"
pinned_fingerprint = ""

[clipboard]
enabled = true
text_only = true
max_bytes = 1048576
```

Linux config:

```toml
device_name = "Lua"
role = "receiver"
listen = "0.0.0.0:42420"
allow_pairing = false
monitor = "eDP-1"

[input]
backend = "libei"

[clipboard]
enabled = true
text_only = true
max_bytes = 1048576
```

Pairing mode:

```bash
edge-receiver-linux --pair
```

This temporarily allows one untrusted Windows controller to pair, then stores its fingerprint and disables pairing again.

## Windows Controller Details

Input capture:

- Use Raw Input for relative mouse movement.
- Use `WH_KEYBOARD_LL` for keyboard capture/suppression.
- Use `WH_MOUSE_LL` for button/wheel suppression and edge observation.
- Keep a message loop alive permanently.

Remote activation:

- On edge crossing, switch to `RemoteActive`.
- Store the local Windows cursor restore position.
- Clip local cursor to a tiny safe area or suppress local motion.
- Send `EnterRemote`.
- Begin forwarding deltas/buttons/keys.

Release behavior:

- `Ctrl+Alt+Pause` always releases to `LocalActive`.
- On release, send `AllKeysUp`.
- Unclip cursor.
- Restore Windows cursor to the appropriate edge.
- Clear all tracked pressed keys/buttons.

Windows tray app:

- Tray icon states: disconnected, connected-local, remote-active, error.
- Menu items:
  - Connect/disconnect
  - Release control
  - Pair/re-pair laptop
  - Open config
  - Quit
- Logs written to a file under `%LOCALAPPDATA%\edge-kvm\logs`.

Clipboard:

- Use Windows clipboard listener APIs.
- Only sync Unicode text.
- Debounce changes for 150 ms.
- Ignore clipboard changes caused by remote sync using sequence IDs.

## Linux Receiver Details

Startup:

- Load config.
- Create/load persistent identity key.
- Listen on `0.0.0.0:42420`.
- Report Hyprland monitor geometry using `hyprctl monitors -j`.
- Initialize `libei` input session.

Input injection:

- Pointer motion: inject relative movement through `libei`.
- Buttons: map left/right/middle/back/forward to Linux button codes.
- Wheel: inject vertical/horizontal scroll.
- Keyboard: receive Linux evdev keycodes and inject key up/down.
- On disconnect or release: inject `AllKeysUp`.

Clipboard:

- Read local text clipboard with `wl-paste`.
- Write remote text clipboard with `wl-copy`.
- Implement polling first at 500 ms interval.
- Later improvement can use `wl-paste --watch`.

## Edge Geometry

For the default layout:

```text
Windows screen: controller local space.
Laptop screen: virtual remote space 1920x1080.
```

Entry from Windows to laptop:

- Cursor crosses Windows left edge.
- Compute normalized Y:

```text
normalized_y = windows_cursor_y / windows_screen_height
```

- Laptop virtual cursor starts at:

```text
x = laptop_width - 2
y = normalized_y * laptop_height
```

Return from laptop:

- If laptop virtual cursor `x >= laptop_width - 1`, switch back to Windows local.
- Restore Windows cursor to:

```text
x = 1
y = normalized_y * windows_screen_height
```

Clamp all coordinates to screen bounds.

## Keyboard Mapping

Implement a fixed mapping table:

```text
Windows scan code + extended flag -> Linux evdev code
```

MVP coverage:

- Letters A-Z
- Numbers 0-9
- Enter, Escape, Backspace, Tab, Space
- Shift, Ctrl, Alt, Super
- Arrow keys
- Function keys F1-F12
- Common punctuation keys
- Delete, Home, End, PageUp, PageDown
- Mouse buttons and wheel

Out of scope for MVP:

- Media keys
- IME-specific behavior
- Multiple keyboard layouts
- UAC/admin desktop control
- macOS
- GNOME/KDE support

## Failure Handling

Hard requirements:

- If TCP disconnects during `RemoteActive`, immediately release Windows back to `LocalActive`.
- If heartbeat is missed for more than 750 ms, release locally.
- Send `AllKeysUp` on release, disconnect, error, and process shutdown.
- Never require killing the app to recover the mouse.
- Release hotkey must work even when remote is active.
- If Linux input backend fails, Windows must refuse remote activation and show an error.

Heartbeat:

```text
Send every 250 ms.
Timeout after 750 ms.
```

Latency target:

```text
Average input event delivery under 20 ms on LAN.
No visible cursor stutter under normal Wi-Fi conditions.
```

## Implementation Phases

1. Scaffold workspace and shared protocol/config crates.
2. Build Linux `libei` proof-of-life:
   - Start receiver.
   - Inject test pointer movement.
   - Inject test click.
   - Inject test key.
3. Build Windows Raw Input and hook prototype:
   - Detect edge crossing.
   - Capture mouse deltas.
   - Capture/suppress keyboard while remote-active.
   - Implement release hotkey.
4. Add encrypted TCP session and pairing.
5. Connect Windows input stream to Linux injection.
6. Add geometry and deterministic enter/return behavior.
7. Add text clipboard sync.
8. Add Windows tray app state/menu.
9. Add logging, config files, and useful error messages.
10. Package/run scripts:
    - Windows debug binary.
    - Linux receiver binary.
    - Optional user service later.

## Test Cases

Unit tests:

- Config parse/defaults.
- Protocol encode/decode round trip.
- Noise pairing rejects unknown changed fingerprints.
- Geometry entry/exit mapping.
- Coordinate clamping.
- Key mapping table correctness for MVP keys.
- Clipboard sequence loop prevention.

Linux local tests:

- `edge-receiver-linux --test-input pointer`
- `edge-receiver-linux --test-input click`
- `edge-receiver-linux --test-input key`
- `edge-receiver-linux --test-clipboard`

Windows local tests:

- Edge detection without network.
- Release hotkey from remote-active state.
- Hook suppression only while remote-active.
- Clipboard listener receives text changes.
- Tray menu release works.

End-to-end tests:

- Windows cursor exits left edge and appears on laptop.
- Laptop virtual cursor exits right edge and returns to Windows.
- Left/right/middle click works on laptop.
- Typing normal text works on laptop.
- Ctrl+C/Ctrl+V style shortcuts work on laptop.
- Text copied on Windows appears on laptop.
- Text copied on laptop appears on Windows.
- Pull network cable/disable Wi-Fi while remote-active: Windows releases locally within 750 ms.
- Kill Linux receiver while remote-active: Windows releases locally and no keys remain stuck.
- Press/release hotkey repeatedly: never traps cursor.

## Acceptance Criteria

The first version is accepted when:

- Windows controls the Hyprland laptop with mouse and keyboard.
- Cursor can enter and leave using the configured screen edge.
- The mouse cannot remain trapped after disconnect or backend failure.
- `Ctrl+Alt+Pause` always returns control to Windows.
- Text clipboard sync works both ways.
- Pairing persists across restarts.
- Changed peer key is rejected until re-paired.
- Logs clearly explain connection, pairing, backend, and firewall errors.

## Explicit Assumptions And Defaults

- Build language: Rust.
- Project path: `/home/lua/Desktop/edge-kvm`.
- Windows version: Windows 11.
- Laptop position: left of Windows.
- Linux target: this Hyprland laptop only.
- Linux input backend: `libei` first.
- Clipboard scope: text only.
- Port: `42420/tcp`.
- Security: pinned encrypted pairing.
- UI: Windows tray app plus Linux background receiver.
- Emergency release hotkey: `Ctrl+Alt+Pause`.
- No UAC/admin desktop support in MVP.
- No multi-monitor support in MVP beyond using the primary Windows monitor and laptop `eDP-1`.
