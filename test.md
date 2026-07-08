# edge-kvm Test Checklist

This file is written for an AI/helper on the Windows PC to verify the current
end-to-end test build against the Linux receiver.

## Current Linux Receiver

The Linux receiver was started on the laptop with:

```bash
cd /home/lua/Desktop/edge-kvm
cargo run -p edge-receiver-linux -- --pair
```

Expected receiver state:

```text
listening on 0.0.0.0:42420
LAN IP: 192.168.0.11
allow_pairing=true
```

The receiver may log this warning:

```text
libei was not found through pkg-config; using log-only input backend for testing
```

That is acceptable for this test. It means input events are logged on Linux
instead of injected into the desktop.

## Windows Setup

From the Windows repo checkout:

```powershell
git pull
git rev-parse --short HEAD
```

Expected commit must be at least:

```text
dd95d02
```

Build the controller:

```powershell
cargo build -p edge-controller-win --release
```

Create a portable folder:

```powershell
mkdir portable-windows -Force
copy target\release\edge-controller-win.exe portable-windows\
cd portable-windows
```

Run once to create portable config/state:

```powershell
.\edge-controller-win.exe --help
.\edge-controller-win.exe --dry-run
```

If `controller.toml` does not exist yet, run:

```powershell
.\edge-controller-win.exe
```

Then edit:

```powershell
notepad .\controller.toml
```

Set:

```toml
[peer.laptop]
host = "192.168.0.11"
port = 42420
position = "left"
```

Keep:

```toml
[input]
backend = "auto"
```

## Connectivity Check

Preferred PowerShell check:

```powershell
Test-NetConnection 192.168.0.11 -Port 42420
```

Pass condition:

```text
TcpTestSucceeded : True
```

If this fails, the issue is network/firewall/routing, not the Rust protocol.

If `Test-NetConnection` does not exist, use this PowerShell fallback:

```powershell
$client = New-Object System.Net.Sockets.TcpClient
$async = $client.BeginConnect("192.168.0.11", 42420, $null, $null)
if ($async.AsyncWaitHandle.WaitOne(3000)) {
    $client.EndConnect($async)
    "TCP connect: OK"
} else {
    "TCP connect: TIMEOUT"
}
$client.Close()
```

Pass condition:

```text
TCP connect: OK
```

If running from `cmd.exe`, use:

```cmd
powershell -NoProfile -Command "$c=New-Object Net.Sockets.TcpClient;$a=$c.BeginConnect('192.168.0.11',42420,$null,$null);if($a.AsyncWaitHandle.WaitOne(3000)){$c.EndConnect($a);'TCP connect: OK'}else{'TCP connect: TIMEOUT'};$c.Close()"
```

If `curl.exe` is available, this is also acceptable:

```powershell
curl.exe telnet://192.168.0.11:42420 --connect-timeout 3
```

For `curl.exe`, a successful TCP connection may hang or print a protocol-related
error because this is not HTTP. That still proves the port is reachable. A
timeout means the TCP path is blocked.

## Protocol Tests

Run these from `portable-windows`:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

Expected Windows-side signs:

```text
sent encrypted controller hello
receiver hello
receiver screen info
dry-run connection succeeded
sent test input
```

Expected Linux receiver logs:

For `--dry-run`:

```text
controller connected
paired new controller
```

For `--test-input pointer`:

```text
received input event event=PointerMotion { dx: 80.0, dy: 0.0 }
received all-keys-up
```

For `--test-input click`:

```text
received input event event=PointerButton { button: Left, down: true }
received input event event=PointerButton { button: Left, down: false }
received all-keys-up
```

For `--test-input key`:

```text
received input event event=Key { evdev_code: 30, down: true }
received input event event=Key { evdev_code: 30, down: false }
received all-keys-up
```

For `--test-clipboard-text "hello from Windows"`:

```text
controller connected
```

If `wl-copy` is available on Linux, clipboard text should be written. If it
fails, report the Linux receiver error text.

## Pass Criteria

The current build passes this phase when:

- Windows builds `edge-controller-win`.
- Windows reaches `192.168.0.11:42420`.
- `--dry-run` completes successfully.
- Pointer, click, and key test commands appear in Linux receiver logs.
- No command requires `%APPDATA%`; `controller.toml` and `state\` stay beside
  `edge-controller-win.exe`.

## Known Limit

This does not yet prove real desktop input injection on Linux. On this laptop,
`libei` is not available through `pkg-config`, so the receiver intentionally uses
the log-only backend. The next implementation step is the real Linux injection
backend.
