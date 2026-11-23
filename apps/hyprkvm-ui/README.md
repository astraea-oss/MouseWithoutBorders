# hyprKvm desktop UI

Tauri 2 + SvelteKit (static) UI for the edge-kvm backend. It replaces the
egui windows during the migration described in
[`docs/plans/hyprkvm-migration.md`](../../docs/plans/hyprkvm-migration.md).

The backend stays the only owner of sessions, identity, pins and settings
and keeps running when this window closes or crashes. The UI talks to it
over the local control endpoint using `crates/edge-control`, and acts only
through the explicit commands in `src-tauri/src/commands.rs`.

## Status

| Area | State |
| --- | --- |
| Home (status, tiles, Arrangement, Sharing, Devices, Notifications, This computer, Advanced) | Working on Linux against the new backend |
| Add a phone popup | Working on Linux; closing it withdraws the code |
| Confirm computer popup | Not yet: computer pairing still confirms in the egui window (M3) |
| View | Shown as unavailable; not started (M4/M5) |
| Windows | Builds, but the Windows backend has no control endpoint yet, so the UI reports it as not running |

## Develop

```sh
npm install            # copies packages/hypr-ui in (install-links), with its fonts
npm run check          # svelte-check, warnings are errors
npx tauri dev          # Vite dev server + debug app
npx tauri build --no-bundle
```

After editing `packages/hypr-ui`, run `npm run ui:refresh` so the copied
package is updated.

The executable is `hyprkvm`. Flags:

- `--route home|view|pair-phone`: which window to show. A second launch
  routes the running instance instead of starting another.
- `--config <path>` (or `EDGE_KVM_CONFIG`): attach only to the backend using
  this settings file. A backend for another portable folder is reported,
  not silently driven.

## Tray

During the migration the backend's tray still opens the egui windows. Set
`EDGE_KVM_DESKTOP_UI=hyprkvm` in the backend's environment (and put
`hyprkvm` next to `edge-receiver-linux`) to have Settings… open Home and
Add phone… open the popup instead. For the user service:

```sh
systemctl --user edit edge-kvm-receiver.service
# [Service]
# Environment=EDGE_KVM_DESKTOP_UI=hyprkvm
```

## Hyprland

The windows have no decorations on Linux; Hyprland's border is the frame.
Every window has the class `hyprkvm` (GTK gives the app one Wayland app ID,
so popups can't have their own class). Match the popup by title instead:

```lua
hl.window_rule({
    name = "hyprkvm-pair",
    match = { class = "^hyprkvm$", title = "^Add a phone · hyprKvm$" },
    float = true,
    center = true,
})
```

(`hyprland.conf` syntax: `windowrule = float, center, class:^(hyprkvm)$, title:^(Add a phone · hyprKvm)$`.)
Matching on `class` with `float` was checked on Hyprland 0.57.2; the title
match and `center` were not.
