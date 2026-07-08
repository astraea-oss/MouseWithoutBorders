# Windows Test Result

Date: 2026-07-08
Windows commit tested: `23a22a0`

## Summary

The Windows build, portable setup, and TCP reachability check now succeed. The
end-to-end protocol tests reach the Linux receiver, but the receiver rejects the
Windows controller because its pinned peer fingerprint does not match this
portable Windows identity.

## Passed

- `git pull`: already up to date
- `git rev-parse --short HEAD`: `23a22a0`
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

## Failed

The controller protocol commands all reached the receiver, but failed during
pairing:

```powershell
.\edge-controller-win.exe --dry-run
.\edge-controller-win.exe --test-input pointer
.\edge-controller-win.exe --test-input click
.\edge-controller-win.exe --test-input key
.\edge-controller-win.exe --test-clipboard-text "hello from Windows"
```

Each command returned:

```text
receiver error: pin_mismatch: peer key changed for Main PC:
pinned 1770a1ad:8e6da14d:a9c40a9d:7afd7278
got    f3c5eebe:41c1bfe1:97001a2b:e3578264
```

## Interpretation

Windows can reach `192.168.0.11:42420`, and the Linux receiver is accepting
connections. The current blocker is not TCP reachability; it is the receiver's
pinned controller identity.

The Linux receiver has `"Main PC"` pinned to:

```text
1770a1ad:8e6da14d:a9c40a9d:7afd7278
```

The Windows `portable-windows\state\identity.toml` identity presented:

```text
f3c5eebe:41c1bfe1:97001a2b:e3578264
```

To continue testing, the Linux side needs to re-pair this Windows identity or
clear/update its pinned peer entry for `"Main PC"`.

No `edge-controller-win.exe` process was left running after the test.
