# Windows Instructions

Run these from the repo checkout on Windows:

```powershell
git pull
git rev-parse --short HEAD
cargo build -p edge-controller-win --release
copy target\release\edge-controller-win.exe portable-windows\
cd portable-windows
.\edge-controller-win.exe
```

Expected commit should be `3526a4d` or newer.
