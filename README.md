# edge-kvm

Personal Windows-to-Hyprland software KVM prototype.

This workspace follows `PLAN.md` and is intentionally narrow:

- Windows controller owns the physical keyboard and mouse.
- Linux receiver runs on the Hyprland laptop.
- Protocol frames are length-prefixed MessagePack.
- Pairing uses persistent device identities and pinned peer fingerprints.
- Portable by default: configs and state live beside the running executable.
- Linux input detects CachyOS/Arch `libei-1.0`, but real sender injection is not
  implemented yet. `backend = "auto"` currently falls back to log-only input so
  the encrypted network path can be tested honestly.

## Build

```bash
cargo test --workspace
```

## Linux receiver

For development:

```bash
cargo run -p edge-receiver-linux -- --pair
```

For portable use, build and copy the binary to a folder you control:

```bash
cargo build -p edge-receiver-linux --release
mkdir -p ./portable-linux
cp target/release/edge-receiver-linux ./portable-linux/
cd ./portable-linux
./edge-receiver-linux --pair
```

On first run it creates:

```text
receiver.toml
state/
```

Useful checks:

```bash
cargo run -p edge-receiver-linux -- --test-clipboard
cargo run -p edge-receiver-linux -- --test-input pointer
```

With `input.backend = "auto"`, `--test-input` logs events. The receiver detects
`libei-1.0` on CachyOS/Arch, but real local injection is still a development
task. Set `input.backend = "libei"` only once the sender backend is implemented.

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
forwarded relative motion.

On non-Windows hosts, use `--dry-run` to validate config and the initial protocol hello.

## End-to-end test

Start the Linux receiver:

```bash
./edge-receiver-linux --pair
```

From Windows, send test events:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

Expected result with `backend = "auto"`: the receiver stays connected and logs
each received input or clipboard event. Real Hyprland injection is the next
backend implementation step.
