# edge-kvm

Personal Windows-to-Hyprland software KVM prototype.

This workspace follows `PLAN.md` and is intentionally narrow:

- Windows controller owns the physical keyboard and mouse.
- Linux receiver runs on the Hyprland laptop.
- Protocol frames are length-prefixed MessagePack.
- Pairing uses persistent device identities and pinned peer fingerprints.
- Linux input injection fails closed until a real `libei` FFI backend is wired in.

## Build

```bash
cargo test --workspace
```

## Linux receiver

```bash
cargo run -p edge-receiver-linux -- --pair
```

Useful checks:

```bash
cargo run -p edge-receiver-linux -- --test-clipboard
cargo run -p edge-receiver-linux -- --test-input pointer
```

The `--test-input` commands require `libei` to be discoverable through `pkg-config`.

## Windows controller

```powershell
cargo run -p edge-controller-win
```

On non-Windows hosts, use `--dry-run` to validate config and the initial protocol hello.

