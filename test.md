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
failed to initialize libei; trying Hyprland virtual input backend
```

That is acceptable on this CachyOS/Hyprland laptop. The installed portal does
not expose RemoteDesktop/ConnectToEIS, so `auto` falls back to the Hyprland
Wayland virtual input backend. The receiver should then log:

```text
using Hyprland Wayland virtual input backend
using Hyprland virtual input backend
```

## Windows Setup

From the Windows repo checkout:

```powershell
git pull
git rev-parse --short HEAD
```

Expected commit must be at least:

```text
3bbd04f
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

For this laptop, `backend = "hyprland"` is also valid and forces the tested
Hyprland virtual input path.

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
controller connected
```

The Linux cursor should move.

For `--test-input click`:

```text
controller connected
```

The Linux desktop should receive a left click.

For `--test-input key`:

```text
controller connected
```

The Linux desktop should receive the `a` key. Test with focus in a safe text
field.

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
- Pointer, click, and key test commands complete successfully and affect the
  Linux desktop.
- No command requires `%APPDATA%`; `controller.toml` and `state\` stay beside
  `edge-controller-win.exe`.

## Current Linux Backend

This now proves local desktop injection on Lua's CachyOS/Hyprland laptop through
the Hyprland/wlroots virtual pointer and virtual keyboard Wayland protocols.
The libei path remains in the tree for later portability work, but this machine
currently needs the Hyprland backend because its portal lacks RemoteDesktop/EIS.
