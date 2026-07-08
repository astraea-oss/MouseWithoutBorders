# edge-kvm

Personal Windows-to-Hyprland software KVM prototype.

This workspace follows `PLAN.md` and is intentionally narrow:

- Windows controller owns the physical keyboard and mouse.
- Linux receiver runs on the Hyprland laptop.
- Protocol frames are length-prefixed MessagePack.
- Pairing uses persistent device identities and pinned peer fingerprints.
- Portable by default: configs and state live beside the running executable.
- Linux input injection fails closed until a real `libei` FFI backend is wired in.

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

The `--test-input` commands require `libei` to be discoverable through `pkg-config`.

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

On non-Windows hosts, use `--dry-run` to validate config and the initial protocol hello.
