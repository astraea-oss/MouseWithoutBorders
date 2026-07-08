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
- Real libei sender injection is not implemented yet.
- Until sender injection exists, `auto` deliberately uses the log-only backend
  so end-to-end Windows-to-Linux protocol tests remain honest and safe.
- `input.backend = "libei"` should fail clearly until the sender FFI is wired.

Later portability work:

- Probe multiple pkg-config names if needed, starting with `libei-1.0`.
- Generate or maintain Rust FFI bindings for `/usr/include/libei-1.0/libei.h`.
- Use `liboeffis-1.0` or the compositor-supported connection path if Hyprland
  requires a portal/remote-desktop handshake.
- Keep a strict `libei` mode that fails if real injection cannot be initialized.
- Keep `auto` mode useful for diagnostics by falling back to log-only with a
  clear warning.

