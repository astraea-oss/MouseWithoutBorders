# Windows Test Result

Date: 2026-07-08
Windows commit tested: `87e0af5`

## Summary

The Windows build and portable setup succeeded, but the end-to-end protocol tests
could not reach the Linux receiver.

## Passed

- `git pull`: already up to date
- `git rev-parse --short HEAD`: `87e0af5`
- `cargo build -p edge-controller-win --release`: passed
- Created/updated `portable-windows`
- `portable-windows\edge-controller-win.exe --help`: passed
- `portable-windows\controller.toml` uses:

```toml
[peer.laptop]
host = "192.168.0.11"
port = 42420
position = "left"

[input]
backend = "auto"
```

## Failed

TCP reachability failed:

```powershell
Test-NetConnection 192.168.0.11 -Port 42420
```

Result:

```text
TcpTestSucceeded : False
```

The controller protocol commands all failed before sending events:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

Each command returned:

```text
Error: failed to connect to 192.168.0.11:42420

Caused by:
    No connection could be made because the target machine actively refused it. (os error 10061)
```

## Interpretation

Windows can route to `192.168.0.11`, but no process is accepting TCP connections
on `192.168.0.11:42420`.

Likely Linux-side causes:

- `edge-receiver-linux` is not running.
- The receiver exited or crashed after startup.
- The receiver is listening on a different address or port.
- A firewall is rejecting connections to port `42420`.

No `edge-controller-win.exe` process was left running after the test.
