use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use edge_audio::{
    AudioPacket, FLAG_PROBE, FRAME_MS, JitterBuffer, MAX_DATAGRAM_BYTES, PacketCipher, PcmCodec,
    PcmConcealer, SAMPLES_PER_CHANNEL, SAMPLES_PER_FRAME, SessionSecrets,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UdpSocket,
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const VIRTUAL_SINK: &str = "edge_kvm_remote";
const PLAYBACK_QUEUE_TARGET_MS: usize = 40;
const PLAYBACK_QUEUE_FRAMES: usize = PLAYBACK_QUEUE_TARGET_MS / FRAME_MS as usize;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoutingJournal {
    previous_sink: String,
    module_id: u32,
}

pub struct AudioRoutingGuard {
    journal_path: PathBuf,
    journal: Option<RoutingJournal>,
    capture_source: String,
}

impl AudioRoutingGuard {
    pub async fn activate(state_dir: &Path, redirect: bool) -> Result<Self> {
        let journal_path = state_dir.join("audio-routing.toml");
        recover_routing(&journal_path).await?;
        let previous_sink = pactl(&["get-default-sink"]).await?.trim().to_string();
        anyhow::ensure!(
            !previous_sink.is_empty(),
            "PipeWire has no default audio sink"
        );
        if !redirect {
            return Ok(Self {
                journal_path,
                journal: None,
                capture_source: format!("{previous_sink}.monitor"),
            });
        }

        let module = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={VIRTUAL_SINK}"),
            "format=float32le",
            "rate=48000",
            "channels=2",
            "channel_map=front-left,front-right",
            "sink_properties=device.description=edge-kvm-remote",
        ])
        .await?;
        let module_id = module
            .trim()
            .parse::<u32>()
            .context("pactl returned an invalid module id")?;
        let journal = RoutingJournal {
            previous_sink,
            module_id,
        };
        if let Some(parent) = journal_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&journal_path, toml::to_string_pretty(&journal)?).await?;

        if let Err(error) = route_to_virtual_sink().await {
            let _ = restore(&journal).await;
            let _ = tokio::fs::remove_file(&journal_path).await;
            return Err(error);
        }
        Ok(Self {
            journal_path,
            journal: Some(journal),
            capture_source: format!("{VIRTUAL_SINK}.monitor"),
        })
    }

    pub fn capture_source(&self) -> &str {
        &self.capture_source
    }

    pub async fn restore_now(&mut self) -> Result<()> {
        if let Some(journal) = self.journal.take() {
            restore(&journal).await?;
        }
        remove_if_exists(&self.journal_path).await
    }
}

impl Drop for AudioRoutingGuard {
    fn drop(&mut self) {
        let Some(journal) = self.journal.take() else {
            return;
        };
        let journal_path = self.journal_path.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = restore(&journal).await;
                let _ = remove_if_exists(&journal_path).await;
            });
        }
    }
}

async fn route_to_virtual_sink() -> Result<()> {
    pactl(&["set-default-sink", VIRTUAL_SINK]).await?;
    let inputs = pactl(&["list", "short", "sink-inputs"]).await?;
    for input in inputs
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        pactl(&["move-sink-input", input, VIRTUAL_SINK]).await?;
    }
    Ok(())
}

async fn restore(journal: &RoutingJournal) -> Result<()> {
    let _ = pactl(&["set-default-sink", &journal.previous_sink]).await;
    if let Ok(inputs) = pactl(&["list", "short", "sink-inputs"]).await {
        for input in inputs
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            let _ = pactl(&["move-sink-input", input, &journal.previous_sink]).await;
        }
    }
    let _ = pactl(&["unload-module", &journal.module_id.to_string()]).await;
    Ok(())
}

pub async fn recover_portable_routing(state_dir: &Path) -> Result<()> {
    recover_routing(&state_dir.join("audio-routing.toml")).await
}

pub async fn test_audio_route(state_dir: &Path) -> Result<()> {
    let mut routing = AudioRoutingGuard::activate(state_dir, true).await?;
    let mut capture = spawn_capture(routing.capture_source())?;
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    let _ = capture.kill().await;
    routing.restore_now().await
}

async fn recover_routing(path: &Path) -> Result<()> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let journal: RoutingJournal = toml::from_str(&text)?;
    restore(&journal).await?;
    remove_if_exists(path).await
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn pactl(arguments: &[&str]) -> Result<String> {
    let output = Command::new("pactl")
        .args(arguments)
        .output()
        .await
        .with_context(|| format!("failed to run pactl {}", arguments.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "pactl {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Opens the UDP path in both directions and returns the controller endpoint as
/// observed by Linux. Using the observed source port keeps audio working across
/// host firewalls and NAT instead of trusting the port in the TCP control frame.
pub async fn establish_peer(
    socket: &UdpSocket,
    cipher: &PacketCipher,
    advertised_destination: SocketAddr,
    expected_ip: IpAddr,
    timeout: Duration,
) -> Result<SocketAddr> {
    let probe = cipher.seal(&AudioPacket {
        sequence: u64::MAX,
        sample_timestamp: 0,
        flags: FLAG_PROBE,
        payload: Vec::new(),
    })?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffer = vec![0; MAX_DATAGRAM_BYTES];

    loop {
        socket
            .send_to(&probe, advertised_destination)
            .await
            .context("failed to send Linux audio UDP probe")?;

        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("timed out establishing the authenticated UDP audio path");
        }
        let wait = (deadline - now).min(Duration::from_millis(250));
        match tokio::time::timeout(wait, socket.recv_from(&mut buffer)).await {
            Ok(Ok((length, source))) if source.ip() == expected_ip => {
                if let Ok(packet) = cipher.open(&buffer[..length])
                    && packet.flags & FLAG_PROBE != 0
                    && packet.payload.is_empty()
                {
                    return Ok(source);
                }
            }
            Ok(Ok(_)) | Err(_) => {}
            Ok(Err(error)) => {
                return Err(error).context("failed to receive Windows audio UDP probe");
            }
        }
    }
}

pub struct LinuxAudioSender {
    task: JoinHandle<()>,
    routing: AudioRoutingGuard,
}

pub struct LinuxAudioReceiver {
    task: Option<JoinHandle<String>>,
}

impl Drop for LinuxAudioReceiver {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl LinuxAudioReceiver {
    pub async fn start(
        socket: Arc<UdpSocket>,
        source_endpoint: SocketAddr,
        secrets: SessionSecrets,
        jitter_target_ms: u32,
    ) -> Result<Self> {
        let mut playback = spawn_playback()?;
        let mut stdin = playback.stdin.take().context("pacat stdin was not piped")?;
        let cipher = PacketCipher::new(&secrets);
        let probe = cipher.seal(&AudioPacket {
            sequence: 0,
            sample_timestamp: 0,
            flags: FLAG_PROBE,
            payload: Vec::new(),
        })?;
        socket
            .send_to(&probe, source_endpoint)
            .await
            .context("failed to send Linux playback UDP probe")?;

        let task = tokio::spawn(async move {
            let mut jitter = JitterBuffer::new(jitter_target_ms);
            let mut concealer = PcmConcealer::default();
            let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
            // pacat deliberately paces writes to the physical audio device. Keep
            // that backpressure off the UDP receive loop so short scheduler or
            // sender bursts are absorbed here instead of overflowing the kernel
            // socket queue and turning into audible packet loss.
            // The jitter buffer already absorbs network variation. Keep this
            // queue small so pacat backpressure cannot turn into audible lag.
            let (playback_tx, mut playback_rx) = mpsc::channel::<Vec<u8>>(PLAYBACK_QUEUE_FRAMES);
            let mut playback_writer = tokio::spawn(async move {
                while let Some(encoded) = playback_rx.recv().await {
                    stdin
                        .write_all(&encoded)
                        .await
                        .map_err(|error| format!("Linux audio playback failed: {error}"))?;
                }
                Ok::<(), String>(())
            });
            let mut probe_retry = tokio::time::interval(Duration::from_millis(250));
            let mut watchdog = tokio::time::interval(Duration::from_millis(500));
            let started = tokio::time::Instant::now();
            let mut last_media = started;
            let mut received_media = false;
            let mut media_stalled = false;
            let mut playback_queue_drops = 0_u64;
            let mut last_queue_warning = started;
            let reason = 'receive: loop {
                tokio::select! {
                    received = socket.recv_from(&mut buffer) => {
                        match received {
                            Ok((length, source)) if source.ip() == source_endpoint.ip() => match cipher.open(&buffer[..length]) {
                                Ok(packet) if packet.flags & FLAG_PROBE == 0 => {
                                    received_media = true;
                                    last_media = tokio::time::Instant::now();
                                    if media_stalled {
                                        tracing::info!("Linux audio media recovered after a UDP gap");
                                        media_stalled = false;
                                    }
                                    if jitter.push(packet) {
                                        for _ in 0..8 {
                                            let Some(packet) = jitter.pop_ready() else { break; };
                                            let pcm = match concealer.decode(packet.as_ref().map(|packet| packet.payload.as_slice())) {
                                                Ok(pcm) => pcm,
                                                Err(error) => {
                                                    tracing::debug!(%error, "rejected PCM audio frame");
                                                    continue;
                                                }
                                            };
                                            let encoded = match PcmCodec::encode(&pcm) {
                                                Ok(encoded) => encoded,
                                                Err(error) => break 'receive format!("Linux PCM playback encoding failed: {error}"),
                                            };
                                            match playback_tx.try_send(encoded) {
                                                Ok(()) => {}
                                                Err(mpsc::error::TrySendError::Full(_)) => {
                                                    playback_queue_drops = playback_queue_drops.saturating_add(1);
                                                    if last_queue_warning.elapsed() >= Duration::from_secs(1) {
                                                        tracing::warn!(
                                                            dropped_frames = playback_queue_drops,
                                                            "Linux audio playback queue saturated; dropping newest frame"
                                                        );
                                                        playback_queue_drops = 0;
                                                        last_queue_warning = tokio::time::Instant::now();
                                                    }
                                                }
                                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                                    break 'receive "Linux audio playback queue closed".to_string();
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => tracing::debug!(%error, "rejected Linux audio datagram"),
                            },
                            Ok(_) => {}
                            Err(error) => break format!("Linux audio UDP receive failed: {error}"),
                        }
                    }
                    _ = probe_retry.tick(), if !received_media => {
                        if let Err(error) = socket.send_to(&probe, source_endpoint).await {
                            break format!("Linux audio UDP probe retry failed: {error}");
                        }
                    }
                    _ = watchdog.tick() => {
                        if received_media
                            && !media_stalled
                            && last_media.elapsed() > Duration::from_secs(2)
                        {
                            // UDP can disappear briefly on Wi-Fi while the encrypted
                            // control session remains healthy. Keep pacat and the
                            // negotiated keys alive so playback resumes naturally when
                            // media returns instead of permanently killing the route.
                            media_stalled = true;
                            tracing::warn!("Linux audio media paused after a UDP gap; waiting for recovery");
                        }
                        if !received_media && started.elapsed() > Duration::from_secs(8) {
                            break "audio source did not start within 8 seconds".to_string();
                        }
                    }
                    result = &mut playback_writer => {
                        break match result {
                            Ok(Ok(())) => "Linux audio playback stopped unexpectedly".to_string(),
                            Ok(Err(error)) => error,
                            Err(error) => format!("Linux audio playback task failed: {error}"),
                        };
                    }
                }
            };
            playback_writer.abort();
            let _ = playback.kill().await;
            reason
        });
        Ok(Self { task: Some(task) })
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|task| task.is_finished())
    }

    pub async fn failure_reason(mut self) -> String {
        let Some(task) = self.task.take() else {
            return "Linux audio receiver stopped without a result".to_string();
        };
        match task.await {
            Ok(reason) => reason,
            Err(error) => format!("Linux audio receiver task failed: {error}"),
        }
    }
}

impl Drop for LinuxAudioSender {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LinuxAudioSender {
    pub async fn start(
        socket: Arc<UdpSocket>,
        destination: std::net::SocketAddr,
        secrets: SessionSecrets,
        state_dir: &Path,
        redirect: bool,
    ) -> Result<Self> {
        let mut routing = AudioRoutingGuard::activate(state_dir, redirect).await?;
        let mut capture = spawn_capture(routing.capture_source())?;
        let mut stdout = capture
            .stdout
            .take()
            .context("parec stdout was not piped")?;
        let cipher = PacketCipher::new(&secrets);
        let (first_packet_tx, first_packet_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut frame = vec![0; SAMPLES_PER_FRAME * 4];
            let mut payload = Vec::with_capacity(edge_audio::PCM_BYTES_PER_FRAME);
            let mut sequence = 1_u64;
            let mut timestamp = 0_u32;
            let mut first_packet_tx = Some(first_packet_tx);
            loop {
                if let Err(error) = stdout.read_exact(&mut frame).await {
                    tracing::warn!(%error, "Linux audio capture ended");
                    break;
                }
                if let Err(error) = PcmCodec::encode_f32le_into(&frame, &mut payload) {
                    tracing::warn!(%error, "PCM encoding failed");
                    break;
                }
                let datagram = match cipher.seal_payload(sequence, timestamp, 0, &payload) {
                    Ok(packet) => packet,
                    Err(error) => {
                        tracing::warn!(%error, "audio packet encryption failed");
                        break;
                    }
                };
                if let Err(error) = socket.send_to(&datagram, destination).await {
                    tracing::warn!(%error, "audio UDP send failed");
                    break;
                }
                if let Some(started) = first_packet_tx.take() {
                    let _ = started.send(());
                    tracing::info!(%destination, "sent first encrypted Linux audio packet");
                }
                sequence = sequence.wrapping_add(1);
                timestamp = timestamp.wrapping_add(SAMPLES_PER_CHANNEL as u32);
            }
            let _ = capture.kill().await;
        });
        match tokio::time::timeout(Duration::from_secs(3), first_packet_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                task.abort();
                let _ = routing.restore_now().await;
                anyhow::bail!("Linux audio capture ended before sending its first packet");
            }
            Err(_) => {
                task.abort();
                let _ = routing.restore_now().await;
                anyhow::bail!("Linux audio capture produced no media for 3 seconds");
            }
        }
        Ok(Self { task, routing })
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(mut self) -> Result<()> {
        self.task.abort();
        self.routing.restore_now().await
    }
}

fn spawn_capture(source: &str) -> Result<Child> {
    let mut command = Command::new("parec");
    command
        .args([
            &format!("--device={source}"),
            "--format=float32le",
            "--rate=48000",
            "--channels=2",
            "--latency-msec=5",
            "--process-time-msec=5",
            "--raw",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
        .spawn()
        .context("failed to start parec; install PipeWire PulseAudio tools")
}

fn spawn_playback() -> Result<Child> {
    let mut command = Command::new("pacat");
    command
        .args([
            "--playback",
            "--format=s16le",
            "--rate=48000",
            "--channels=2",
            "--latency-msec=20",
            "--raw",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
        .spawn()
        .context("failed to start pacat; install PipeWire PulseAudio tools")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticated_probe_uses_observed_peer_endpoint() {
        let linux = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let windows = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let windows_addr = windows.local_addr().unwrap();
        let secrets = SessionSecrets::generate();
        let linux_cipher = PacketCipher::new(&secrets);
        let windows_cipher = PacketCipher::new(&secrets);

        let handshake = establish_peer(
            &linux,
            &linux_cipher,
            windows_addr,
            windows_addr.ip(),
            Duration::from_secs(1),
        );
        let peer = async {
            let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
            let (length, linux_addr) = windows.recv_from(&mut buffer).await.unwrap();
            let probe = windows_cipher.open(&buffer[..length]).unwrap();
            assert_ne!(probe.flags & FLAG_PROBE, 0);

            let response = windows_cipher
                .seal(&AudioPacket {
                    sequence: 0,
                    sample_timestamp: 0,
                    flags: FLAG_PROBE,
                    payload: Vec::new(),
                })
                .unwrap();
            windows.send_to(&response, linux_addr).await.unwrap();
        };

        let (observed, ()) = tokio::join!(handshake, peer);
        assert_eq!(observed.unwrap(), windows_addr);
    }
}
