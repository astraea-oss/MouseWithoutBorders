# Bidirectional Clipboard Image Synchronization

## Summary

The encrypted clipboard channel synchronizes static raster images automatically
between the Windows controller and Linux receiver. A single supported image
file copied in Windows Explorer is decoded locally and treated as an in-memory
image; its file path is never transferred. Other clipboard files and file lists
remain out of scope.

## Selected behavior

- Advertise the forward-compatible `clipboard-image-v1` Hello extension.
- Prefer a supported image over ancillary HTML or text on the same clipboard.
- Decode Windows bitmap data and Linux PNG, JPEG, or BMP offers.
- Fall back to one bounded local PNG, JPEG, or BMP file from a Windows
  `CF_HDROP` clipboard item when no bitmap representation is available.
- Normalize transport data to metadata-free RGBA8 PNG.
- Limit canonical PNG payloads to 4 MiB and decoded images to 16,777,216 pixels.
- Split PNGs into 16 KiB authenticated frames so input and control traffic can
  run between chunks.
- Keep one incoming and one outgoing transfer in memory per connection.
- Cancel replaced transfers and expire incomplete incoming transfers after ten
  seconds.
- Hold a forwarded Windows-to-Linux paste behind an active image transfer for
  at most five seconds.
- Keep text synchronization with peers that do not advertise the extension.
- Never persist or log clipboard contents.

## Portable configuration

```toml
[clipboard]
enabled = true
images_enabled = true
max_bytes = 1048576
max_image_bytes = 4194304
```

Legacy configs that lack `images_enabled` are rewritten beside the executable
with image synchronization enabled. No AppData, installation-directory, or
temporary clipboard storage is used.

## Validation

- Unit tests cover PNG canonicalization, JPEG/BMP normalization, identity
  tracking, protocol compatibility, chunk size, reassembly, cancellation,
  expiry, size limits, and config migration.
- Windows and Linux application builds compile from the shared workspace.
- Live acceptance should use generated fixtures or user-driven clipboard
  actions. Do not capture a screenshot of the user's computer without explicit
  permission.
