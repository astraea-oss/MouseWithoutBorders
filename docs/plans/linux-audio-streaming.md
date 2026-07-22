# Linux-to-Windows Audio Streaming

## Summary

Add an opt-in audio channel that redirects all Linux system audio to the Windows main computer whenever the paired devices are connected and audio streaming is enabled.

The first release will:

- Capture the Linux system mix through PipeWire.
- Redirect audio away from the laptop speakers while streaming.
- Encode stereo audio as signed 16-bit PCM at 48 kHz with no native codec dependency.
- Send audio over a separate encrypted UDP channel so audio congestion cannot stall mouse, keyboard, clipboard, or heartbeat traffic.
- Target approximately 60–100 ms end-to-end latency on a normal LAN.
- Play through the current Windows default multimedia output and follow device changes.
- Provide an audio toggle in both tray menus and the dark-mode settings UI.
- Store every preference, recovery journal, identity, and log beside the executable; never use AppData or an install directory.

Audio remains disabled by default after upgrading, preventing an update from unexpectedly rerouting the laptop's sound.

## Architecture

### Workspace additions

Add three crates:

- `crates/edge-audio`: shared PCM conversion, the encrypted UDP packet format, replay protection, packet-loss handling, jitter buffering, and statistics.
- `crates/edge-linux-audio`: PipeWire capture, PipeWire-Pulse routing and restoration, and Linux audio lifecycle and crash recovery.
- `crates/edge-windows-audio`: Windows output through CPAL/WASAPI, default-output monitoring, decoding, resampling, and playback buffering.

Keep audio out of the existing input crates because its real-time lifecycle and failure handling must remain independently testable.

### Media path

```text
Linux applications
    -> temporary edge_kvm_remote PipeWire sink
    -> edge-linux-audio capture
    -> 5 ms stereo PCM frames
    -> signed 16-bit PCM conversion
    -> ChaCha20-Poly1305 UDP packets
    -> Windows jitter buffer
    -> PCM decoder / silence concealment
    -> sample-rate converter
    -> CPAL/WASAPI default output
```

Audio media must never share the ordered TCP stream used for input, clipboard, control, and heartbeats.

## Configuration

Extend `edge_common::AppConfig` with an `AudioConfig`:

```rust
pub struct AudioConfig {
    pub enabled: bool,
    pub local_playback: AudioLocalPlayback,
    pub jitter_target_ms: u32,
}

#[serde(rename_all = "kebab-case")]
pub enum AudioLocalPlayback {
    Redirect,
    Mirror,
}
```

Defaults:

```toml
[audio]
enabled = false
local_playback = "redirect"
jitter_target_ms = 60
```

Requirements:

- Existing configurations deserialize with audio disabled.
- Validate `jitter_target_ms` within `40..=120`.
- Initially expose only the enable and local-playback choices in the settings UI.
- Keep jitter as an advanced TOML option.
- Store configuration, logs, and `state/audio-routing.toml` beside each executable.

## Control Protocol

Retain the authenticated Noise TCP connection for negotiation and state changes. Extend `Hello` with a backward-compatible, `serde(default)` capability list containing `Capability::AudioV1`.

Add `Frame::Audio(AudioControl)` with these messages:

- `Offer { udp_port, codecs }`
- `Start { session_id, session_salt, session_key, codec, frame_ms, jitter_target_ms }`
- `SetEnabled { enabled }`
- `State { state, detail }`
- `Stop { reason }`

The initial format is `AudioCodec::PcmS16Stereo48Khz`. Stream states are `Disabled`, `WaitingForUdp`, `Starting`, `Streaming`, and `Error`.

Compatibility rules:

- Do not send audio messages unless the peer advertises `AudioV1`.
- A peer without the capability retains all existing KVM functionality.
- Reject an unsupported codec as an audio error without ending the KVM session.
- Keep protocol version 1 because the optional capability negotiation makes this extension backward-compatible.

## UDP Transport and Security

### Establishment

1. Linux binds an ephemeral UDP socket and advertises its source port in an encrypted `Offer`.
2. Windows binds an ephemeral UDP socket and generates a fresh session ID, session salt, and 256-bit session key.
3. Windows transfers that material and its UDP receive port in an encrypted `Start` control message.
4. Windows sends an authenticated best-effort probe to the Linux endpoint to establish its outbound UDP flow.
5. Linux derives the destination IP from the authenticated TCP peer and sends encrypted media outbound to the advertised Windows port without waiting for inbound UDP. This avoids requiring a Linux firewall rule for a random media port.
6. Windows connects its socket to the offered Linux endpoint and accepts only authenticated packets for the current session.
7. Rotate keys on every start, reconnect, or UDP restart.

### Packet format

Use a compact, versioned header with magic `EKA1`, version, flags, header length, 128-bit session ID, 64-bit sequence, and 32-bit sample timestamp.

- Encrypt each PCM payload with ChaCha20-Poly1305.
- Build the nonce from the four-byte session salt and 64-bit sequence.
- Authenticate the complete header as additional authenticated data.
- Keep datagrams below 1,200 bytes.
- Reject malformed sizes before allocation.
- Enforce a 128-packet replay window.
- Never log session keys or plaintext audio.
- Zero key material when the stream stops.

## Linux Capture and Redirection

### Preflight

Before modifying routing:

- Confirm that PipeWire and the PipeWire-Pulse server are reachable.
- Confirm that `pactl` can load `module-null-sink`.
- Determine the current default sink or fail without changing routing.
- Surface audio-specific errors while leaving the KVM connection alive.

### Transactional routing

Implement an `AudioRoutingGuard` that:

1. Repairs an unfinished routing journal from an earlier crash.
2. Records the current default sink in `state/audio-routing.toml`.
3. Loads a null sink named `edge_kvm_remote` using `float32le`, 48 kHz, stereo, and front-left/front-right channel mapping.
4. Records the returned module ID before changing the default.
5. Makes the temporary sink the default and moves all existing sink inputs to it.
6. Captures `edge_kvm_remote.monitor` through `pipewire-rs`.
7. Copies only bounded PCM chunks from the real-time callback to a lock-free queue; encoding and networking run on workers.

Normal stop and every failure path must:

1. Stop capture and encoding.
2. Restore the saved default sink.
3. Move streams back to it.
4. Fall back to the current highest-priority real sink if the saved sink disappeared.
5. Unload only the app-owned module instance.
6. Mark the journal clean and remove it.

Run restoration on toggle-off, control disconnect, heartbeat timeout, UDP timeout, shutdown, capture failure, encoder failure, controller replacement, and startup after an unclean exit. Never write PipeWire or WirePlumber configuration into the user profile.

### Encoding

- Capture interleaved stereo `f32` at 48 kHz.
- Encode 240 samples per channel into each 5 ms PCM packet.
- Use interleaved signed 16-bit little-endian samples, requiring approximately 1.54 Mbps before packet overhead.
- Keep packets below 1,200 bytes and avoid native codec libraries or DLLs.
- Bound all PCM and encoded queues.

## Windows Playback

### Receive pipeline

- Authenticate and decrypt before parsing PCM data.
- Buffer packets by sequence and begin playback near 60 ms.
- Adapt between 40 and 120 ms based on late packets and underruns.
- Conceal a missing 5 ms packet with silence while keeping the timeline bounded.
- Drop packets that have missed their playback deadline.
- Reset on session or timestamp discontinuity.
- Drop oldest buffered audio and resynchronize rather than allowing delay to grow indefinitely.

### WASAPI output

Use CPAL's WASAPI backend in shared mode:

- Follow the current default multimedia output.
- Prefer its native format and sample rate.
- Resample decoded 48 kHz stereo PCM on a worker thread when required.
- Feed the callback through a bounded lock-free ring buffer.
- Perform no allocation, logging, locking, decoding, or networking in the callback.
- Emit silence on underrun and drop oldest frames on overflow.
- Poll the default output once per second and rebuild playback when it changes.
- Report missing devices as an audio error and retry without ending the KVM session.

## Runtime Coordination

The Windows controller owns the canonical persisted `audio.enabled` preference.

- Windows sends the desired state after capability negotiation.
- Its tray toggle saves `controller.toml` atomically and applies immediately.
- A connected Linux tray toggle sends `SetEnabled` to Windows.
- Windows persists and echoes the resulting state; Linux mirrors it into `receiver.toml`.
- If files disagree after offline edits, the controller value wins on reconnect.
- Disable the Linux audio action while no controller is connected.
- Make stop idempotent and always restore Linux routing.
- Never let an audio startup or runtime failure terminate KVM functionality.

## Tray and Settings UI

All settings surfaces remain dark-mode-first.

Windows tray additions:

- Audio status: off, streaming, or a short error.
- Checked `Stream Linux audio` action.
- `WindowsTrayCommand::SetAudioEnabled(bool)` and a structured tray-state update API.

Linux tray additions:

- Current audio state, packets sent, queue drops, and last audio error.
- Checked `Stream audio to main PC` action while connected.
- `TrayCommand::SetAudioEnabled(bool)`.

Settings additions:

- `Stream Linux system audio` checkbox.
- `Play on laptop too` checkbox, unchecked by default.
- Read-only `Windows output: Follow system default` text.
- A note that stopping or disconnecting restores the prior Linux output.

## Error Handling and Observability

Add rate-limited audio metrics to the portable logs.

Linux metrics:

- PCM frames captured.
- Packets encoded and sent.
- Queue overruns.
- Routing restorations and fallbacks.
- UDP failures.

Windows metrics:

- Received, rejected, duplicate, late, and lost packets.
- Authentication failures.
- Jitter depth and concealed packet count.
- Playback underruns and overflows.
- Output-device rebuilds.

If authenticated media is absent for two seconds, request one audio-channel restart. A failed restart leaves audio in `Error` while KVM remains connected. Linux restores local routing whenever the control connection disappears.

## Testing

### Shared tests

- Configuration defaults, validation, legacy loading, and TOML round trips.
- Capability compatibility and every audio control MessagePack round trip.
- Packet round trips, tampering, wrong session, replay, truncation, oversize, and nonce uniqueness.
- Jitter ordering, loss, duplication, late delivery, overflow, reset, and sequence wraparound.
- PCM silence, tone, stereo separation, and concealment frame length.

### Linux tests

Use an injectable routing command runner to verify:

- Setup and restoration command sequences.
- Rollback after failure at every setup stage.
- Ownership-safe module unloading.
- Existing-stream movement and disappeared-sink fallback.
- Startup recovery from a stale journal.
- Idempotent stop behavior.
- Non-blocking PipeWire callbacks.

Add `edge-receiver-linux --test-audio-route` to create the temporary sink, capture a short test tone, restore routing, and exit without networking.

### Windows tests

- Drive decoding, resampling, and buffering with synthetic encrypted PCM packets.
- Verify underrun silence and overflow policy.
- Verify default-device rebuild and stale-buffer reset.
- Verify audio failures do not affect input capture.

Add `edge-controller-win.exe --test-audio` to play a local encoded/decoded test tone through the default output.

### End-to-end acceptance

- Current and newly opened Linux applications redirect to Windows.
- Laptop speakers are silent while Windows plays the stream.
- Toggle-off restores the exact previous Linux sink.
- Network loss restores Linux audio within two seconds.
- Restart after a forced receiver exit repairs routing from the journal.
- Windows follows default-output changes.
- Input latency remains unaffected under audio load.
- One percent loss and moderate jitter cause concealment instead of accumulating delay.
- Healthy-LAN latency normally remains within 60–100 ms.
- Neither executable writes app-owned data outside its portable directory.

## Implementation Sequence

1. Add configuration, validation, capabilities, and audio control messages.
2. Build shared packet security, PCM conversion, jitter, and unit tests.
3. Build Windows synthetic receive/decode/playback and output switching.
4. Add Linux PipeWire capture without rerouting and verify Windows playback.
5. Add transactional null-sink routing, restoration, and crash recovery.
6. Integrate UDP negotiation and lifecycle with the Noise session.
7. Add synchronized tray controls and dark settings fields.
8. Add diagnostics, counters, and inactivity recovery.
9. Test under impairment and tune the 60–100 ms balanced profile.
10. Build and verify portable release artifacts.

## Scope

Included:

- One Linux source and one Windows destination.
- All system audio.
- Stereo signed 16-bit PCM, with optional compressed profiles deferred.
- Redirect and mirror behavior.
- LAN operation through the existing paired relationship.
- Default Windows output.

Deferred:

- Microphone or Windows-to-Linux forwarding.
- Per-application selection.
- Surround and spatial metadata.
- Bluetooth-specific latency modes.
- Internet relay or general NAT traversal.
- Multiple listeners.
- Independent volume or mixing controls.

## Known Build Baseline

The current workspace does not run `cargo test --workspace` successfully on Windows because `edge-receiver-linux` imports Unix-only Tokio and socket APIs without target gating. Gate those imports and tests with `cfg(unix)`, or exclude the Linux receiver from the Windows-host workspace test, before making the complete Windows workspace run a release criterion.
