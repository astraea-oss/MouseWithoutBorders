# Windows Test Result

Date: 2026-07-08
Windows commit tested: `71b3180`

## Summary

The Windows build, portable setup, TCP reachability check, and controller
protocol tests all succeeded against the Linux receiver.

## Passed

- `git pull`: already up to date
- `git rev-parse --short HEAD`: `71b3180`
- `cargo build -p edge-controller-win --release`: passed
- Created/updated `portable-windows`
- `portable-windows\edge-controller-win.exe --help`: passed
- `Test-NetConnection 192.168.0.11 -Port 42420`: passed
- `portable-windows\controller.toml` uses:

```toml
[peer.laptop]
host = "192.168.0.11"
port = 42420
position = "left"

[input]
backend = "auto"
```

## Protocol Tests

The controller protocol commands all completed successfully:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

```text
--dry-run                         ExitCode: 0
--test-input pointer              ExitCode: 0
--test-input click                ExitCode: 0
--test-input key                  ExitCode: 0
--test-clipboard-text "hello..."  ExitCode: 0
```

## Interpretation

Windows can reach `192.168.0.11:42420`, the Linux receiver accepts the encrypted
controller session, and the Windows-side test commands complete successfully.

No `edge-controller-win.exe` process was left running after the test.
