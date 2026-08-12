# edge-kvm

Personal Windows-to-Hyprland software KVM prototype.

This workspace follows `PLAN.md` and is intentionally narrow:

- Windows controller owns the physical keyboard and mouse.
- Linux receiver runs on the Hyprland laptop.
- Protocol frames are length-prefixed MessagePack.
- Pairing uses persistent device identities and pinned peer fingerprints.
- Portable by default: configs and state live beside the running executable.
- Linux input uses a kernel `uinput` device on Niri so compositor shortcuts work,
  otherwise trying the optional `libei-1.0` backend before Hyprland's virtual
  input protocols. `backend = "auto"` exits if no real input path initializes.

## Build

```bash
cargo test --workspace
```

## Linux receiver

For development:

```bash
cargo run -p edge-receiver-linux
```

For portable use, build and copy the binary to a folder you control:

```bash
cargo build -p edge-receiver-linux --release
mkdir -p ./portable-linux
cp target/release/edge-receiver-linux ./portable-linux/
cd ./portable-linux
./edge-receiver-linux
```

On first run it creates:

```text
receiver.toml
state/
```

Linux audio streaming uses the PipeWire-Pulse command-line tools `pactl` and
`parec`. On Arch/CachyOS these are normally provided by `libpulse` alongside
`pipewire-pulse`. Verify routing without a Windows connection:

```bash
./edge-receiver-linux --test-audio-route
```

The diagnostic temporarily creates the `edge_kvm_remote` sink and restores the
previous default before exiting.

Useful checks:

```bash
cargo run -p edge-receiver-linux -- --test-clipboard
cargo run -p edge-receiver-linux -- --test-input pointer
```

With `[clipboard].enabled = true`, connected devices automatically synchronize
text clipboard changes in both directions. With `images_enabled = true`, static
clipboard images are normalized to PNG and synchronized too. Linux uses
`wl-paste --watch` and `wl-copy`; Windows uses the native clipboard through
`arboard`. Image payloads are capped by `max_image_bytes` (4 MiB by default),
chunked on the encrypted session, and never written to an app-owned file.
When Windows Explorer contains exactly one copied PNG, JPEG, or BMP file, the
controller reads that bounded local file and sends only its decoded pixels as a
clipboard image. It never sends the source path.

```toml
[clipboard]
enabled = true
images_enabled = true
max_bytes = 1048576
max_image_bytes = 4194304
```

Multiple copied files, arbitrary files, and file paths themselves are
intentionally not transferred.

With `input.backend = "auto"`, `--test-input` uses `uinput` in a Niri session so
synthetic keys pass through Niri's normal compositor-shortcut path. On Hyprland
this is normally the Wayland virtual input backend. Set the backend to `uinput`
to require `/dev/uinput`, or to `log` when testing only the encrypted protocol
without injecting local input.

## Windows controller

For development:

```powershell
cargo run -p edge-controller-win
```

For portable use on Windows:

```powershell
cargo build -p edge-controller-win --release
mkdir portable-windows
copy target\release\edge-controller-win.exe portable-windows\
cd portable-windows
.\edge-controller-win.exe
```

On first run it creates:

```text
controller.toml
state\
```

Edit `controller.toml` in that same folder and set `[peer.laptop].host` to the Linux laptop IP. Nothing is written to `%APPDATA%` unless you explicitly set `EDGE_KVM_CONFIG` or `EDGE_KVM_STATE_DIR` there yourself.

The tray icon opens Settings with a left-click and shows its menu with a
right-click. `input.game_compatibility` controls edge switching while a game is
focused: `always-enabled` (default), `borderless`, or `compatible`. Active
remote mouse movement uses Windows Raw Input so games cannot distort the
forwarded relative motion. Uncheck `Forward mouse and keyboard` in either tray
to pause input without stopping Linux audio or clipboard synchronization;
either side can turn forwarding back on.

### Pairing and changed keys

Normal reconnects use the saved identity keys automatically. For a first
connection, or after intentionally resetting either computer's `state` folder:

1. Choose `Pair or replace controller...` from the Linux tray.
2. Choose `Pair or replace laptop...` from the Windows tray.
3. Compare the six-digit code shown on both computers.
4. Select `Pair` on both only when the codes match.

Neither saved key is replaced until both computers approve. A changed key is
shown with an additional warning. For scripted startup, `--pair` arms the same
one-shot confirmation flow; it no longer trusts the next key automatically.

On non-Windows hosts, use `--dry-run` to validate config and the initial protocol hello.

To verify Windows playback without Linux, run:

```powershell
.\edge-controller-win.exe --test-audio
```

Linux system-audio streaming is enabled by default for new Windows controller
configs. Legacy controller configs without an `[audio]` section are migrated on
startup; an explicit existing preference is preserved. Use Settings or the
checked `Stream Linux audio` tray action to change it while connected. The
initial format is encrypted 48 kHz stereo PCM over UDP, requiring roughly
1.54 Mbps.

## End-to-end test

Start the Linux receiver:

```bash
./edge-receiver-linux
```

From Windows, send test events:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

Expected result with `backend = "auto"`: pointer, click, and key events are
injected into the Linux desktop. If no real input backend can initialize, the
receiver exits with an error instead of appearing healthy in log-only mode.
