# @hyprkvm/hypr-ui

Theme tokens, bundled fonts and small presentational Svelte 5 components
shared by the hyprKvm desktop UI and, later, HyprMobile.

```js
import '@hyprkvm/hypr-ui/fonts.css';
import '@hyprkvm/hypr-ui/theme.css';
import { Card, Tile, Pill, Segment, Switch, Label, StatusDot } from '@hyprkvm/hypr-ui';
```

Rules:

- Components know nothing about devices, permissions, Tauri or platform
  lifecycle. They take values and report changes.
- Red means live (a stream, captured keys) or destructive, and nothing else.

Status: **desktop-owned**. It becomes shared once HyprMobile vendors an
immutable snapshot at a recorded commit and that extraction lands with
unchanged appearance (see `docs/plans/hyprkvm-migration.md`, M2).
