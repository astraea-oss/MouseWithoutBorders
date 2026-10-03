#[cfg(windows)]
mod implementation {
    use std::{
        collections::VecDeque,
        net::{IpAddr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result};
    use edge_audio::{
        AudioPacket, FLAG_PROBE, JitterBuffer, MAX_DATAGRAM_BYTES, PacketCipher, PcmCodec,
        PcmConcealer, SAMPLE_RATE, SAMPLES_PER_CHANNEL, SessionSecrets,
    };
    use edge_audio_output::{AudioPlayer, PlaybackStats};
    use tokio::{
        net::UdpSocket,
        sync::{mpsc, oneshot},
        task::JoinHandle,
        time,
    };
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

    /// Authenticates the UDP endpoint observed on the wire before capture starts.
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
        let deadline = time::Instant::now() + timeout;
        let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
        loop {
            socket
                .send_to(&probe, advertised_destination)
                .await
                .context("failed to send Windows audio UDP probe")?;
            let now = time::Instant::now();
            if now >= deadline {
                anyhow::bail!("timed out establishing the authenticated UDP audio path");
            }
            let wait = (deadline - now).min(Duration::from_millis(250));
            match time::timeout(wait, socket.recv_from(&mut buffer)).await {
                Ok(Ok((length, source))) if source.ip() == expected_ip => {
                    if let Ok(packet) = cipher.open(&buffer[..length])
                        && packet.flags & FLAG_PROBE != 0
                        && packet.payload.is_empty()
                    {
                        return Ok(source);
                    }
                }
                Ok(Ok(_)) | Err(_) => {}
                Ok(Err(error)) => return Err(error).context("failed to receive audio UDP probe"),
            }
        }
    }

    const MAX_PLAYOUT_FRAMES_PER_DATAGRAM: usize = 8;

    #[derive(Debug, Clone, Copy)]
    pub struct WindowsAudioStats {
        pub authenticated_packets: u64,
        pub rejected_packets: u64,
        pub late_packets: u64,
        pub concealed_packets: u64,
        pub output_underruns: u64,
        pub dropped_output_frames: u64,
        pub queued_output_ms: usize,
    }

    pub struct WindowsAudioReceiver {
        task: Option<JoinHandle<String>>,
        linux_streaming: Arc<AtomicBool>,
        stats: Arc<PlaybackStats>,
    }

    pub struct WindowsAudioSender {
        task: Option<JoinHandle<String>>,
        stop: Arc<AtomicBool>,
        capture_thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for WindowsAudioSender {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(task) = self.task.take() {
                task.abort();
            }
            // WASAPI capture is event-driven and observes `stop` within 500 ms.
            // Do not block the async runtime while it winds down.
            self.capture_thread.take();
        }
    }

    impl WindowsAudioSender {
        pub async fn start(
            socket: Arc<UdpSocket>,
            destination: SocketAddr,
            secrets: SessionSecrets,
        ) -> Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let (capture_tx, mut capture_rx) =
                mpsc::channel::<std::result::Result<Vec<u8>, String>>(8);
            let capture_stop = stop.clone();
            let capture_thread = thread::Builder::new()
                .name("edge-kvm-wasapi-loopback".to_string())
                .spawn(move || {
                    if let Err(error) = capture_windows_loopback(capture_tx.clone(), &capture_stop)
                    {
                        let _ = capture_tx.blocking_send(Err(format!("{error:#}")));
                    }
                })
                .context("failed to start Windows audio capture thread")?;

            let cipher = PacketCipher::new(&secrets);
            let (first_packet_tx, first_packet_rx) = oneshot::channel();
            let task = tokio::spawn(async move {
                let mut sequence = 1_u64;
                let mut timestamp = 0_u32;
                let mut payload = Vec::with_capacity(edge_audio::PCM_BYTES_PER_FRAME);
                let mut first_packet_tx = Some(first_packet_tx);
                let silence = vec![0; edge_audio::SAMPLES_PER_FRAME * size_of::<f32>()];
                loop {
                    // A WASAPI loopback endpoint may stop issuing capture events
                    // while the speaker mix is silent. Keep the negotiated media
                    // path alive with a sparse silent frame so the receiver does
                    // not mistake ordinary quiet for a dead sender. Actual WASAPI
                    // errors still arrive through the channel and end the task.
                    let frame =
                        match time::timeout(Duration::from_millis(250), capture_rx.recv()).await {
                            Ok(Some(Ok(frame))) => frame,
                            Ok(Some(Err(error))) => {
                                break format!("Windows audio capture failed: {error}");
                            }
                            Ok(None) => break "Windows audio capture stopped".to_string(),
                            Err(_) => silence.clone(),
                        };
                    if let Err(error) = PcmCodec::encode_f32le_into(&frame, &mut payload) {
                        break format!("Windows PCM encoding failed: {error}");
                    }
                    let datagram = match cipher.seal_payload(sequence, timestamp, 0, &payload) {
                        Ok(datagram) => datagram,
                        Err(error) => break format!("Windows audio encryption failed: {error}"),
                    };
                    if let Err(error) = socket.send_to(&datagram, destination).await {
                        break format!("Windows audio UDP send failed: {error}");
                    }
                    if let Some(started) = first_packet_tx.take() {
                        let _ = started.send(());
                    }
                    sequence = sequence.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(SAMPLES_PER_CHANNEL as u32);
                }
            });
            let mut sender = Self {
                task: Some(task),
                stop,
                capture_thread: Some(capture_thread),
            };
            match time::timeout(Duration::from_secs(3), first_packet_rx).await {
                Ok(Ok(())) => Ok(sender),
                Ok(Err(_)) => {
                    let reason = sender.failure_reason().await;
                    anyhow::bail!(reason)
                }
                Err(_) => {
                    sender.stop.store(true, Ordering::Release);
                    anyhow::bail!("Windows loopback capture produced no media for 3 seconds")
                }
            }
        }

        pub fn is_finished(&self) -> bool {
            self.task.as_ref().is_none_or(|task| task.is_finished())
        }

        pub async fn failure_reason(&mut self) -> String {
            self.stop.store(true, Ordering::Release);
            let Some(task) = self.task.take() else {
                return "Windows audio sender stopped without a result".to_string();
            };
            match task.await {
                Ok(reason) => reason,
                Err(error) => format!("Windows audio sender task failed: {error}"),
            }
        }
    }

    fn capture_windows_loopback(
        tx: mpsc::Sender<std::result::Result<Vec<u8>, String>>,
        stop: &AtomicBool,
    ) -> Result<()> {
        initialize_mta()
            .ok()
            .map_err(|error| anyhow::anyhow!(error))?;
        let enumerator = DeviceEnumerator::new().map_err(|error| anyhow::anyhow!(error))?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .map_err(|error| anyhow::anyhow!(error))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|error| anyhow::anyhow!(error))?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
        let (_, minimum_period) = client
            .get_device_period()
            .map_err(|error| anyhow::anyhow!(error))?;
        client
            .initialize_client(
                &format,
                // The endpoint is a render device, but the stream direction must
                // be Capture for WASAPI to enable loopback mode. Passing Render
                // creates an ordinary playback stream and makes
                // get_audiocaptureclient fail before the first audio frame.
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: minimum_period,
                },
            )
            .map_err(|error| anyhow::anyhow!(error))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|error| anyhow::anyhow!(error))?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|error| anyhow::anyhow!(error))?;
        let mut samples = VecDeque::new();
        let frame_bytes = edge_audio::SAMPLES_PER_FRAME * size_of::<f32>();
        client
            .start_stream()
            .map_err(|error| anyhow::anyhow!(error))?;
        while !stop.load(Ordering::Acquire) {
            if event.wait_for_event(500).is_err() {
                continue;
            }
            capture
                .read_from_device_to_deque(&mut samples)
                .map_err(|error| anyhow::anyhow!(error))?;
            while samples.len() >= frame_bytes {
                let frame = samples.drain(..frame_bytes).collect::<Vec<_>>();
                if tx.blocking_send(Ok(frame)).is_err() {
                    client.stop_stream().ok();
                    return Ok(());
                }
            }
        }
        client
            .stop_stream()
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(())
    }

    impl Drop for WindowsAudioReceiver {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    impl WindowsAudioReceiver {
        pub async fn start(
            socket: UdpSocket,
            linux_endpoint: SocketAddr,
            secrets: SessionSecrets,
            jitter_target_ms: u32,
        ) -> Result<Self> {
            let stats = Arc::new(PlaybackStats::default());
            let player = AudioPlayer::open_default(stats.clone())?;
            socket
                .connect(linux_endpoint)
                .await
                .context("failed to connect the audio UDP socket")?;
            let cipher = PacketCipher::new(&secrets);
            let probe = cipher.seal(&AudioPacket {
                sequence: 0,
                sample_timestamp: 0,
                flags: FLAG_PROBE,
                payload: Vec::new(),
            })?;
            socket
                .send(&probe)
                .await
                .context("failed to send audio UDP probe")?;

            let linux_streaming = Arc::new(AtomicBool::new(false));
            let task_linux_streaming = linux_streaming.clone();
            let task_stats = stats.clone();
            let task = tokio::spawn(async move {
                let initial_output_name = player.output_name().to_string();
                let (output_change_tx, mut output_change_rx) = mpsc::channel(1);
                let output_monitor = tokio::spawn(async move {
                    let mut current_name = initial_output_name;
                    let mut poll = time::interval(Duration::from_secs(1));
                    poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                    poll.tick().await;
                    loop {
                        poll.tick().await;
                        let queried =
                            tokio::task::spawn_blocking(AudioPlayer::default_output_name).await;
                        match queried {
                            Ok(Ok(name)) if name != current_name => {
                                current_name = name.clone();
                                if output_change_tx.send(name).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => {
                                tracing::warn!(%error, "failed to poll Windows default audio output");
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Windows default audio output poll task failed");
                            }
                        }
                    }
                });
                let mut player = player;
                let mut jitter = JitterBuffer::new(jitter_target_ms);
                let mut concealer = PcmConcealer::default();
                let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
                let mut media_watchdog = time::interval(Duration::from_millis(500));
                let mut probe_retry = time::interval(Duration::from_millis(250));
                let receiver_started = Instant::now();
                let mut last_authenticated_media = Instant::now();
                let mut expecting_media = false;
                let mut received_media = false;
                let reason = loop {
                    tokio::select! {
                        received = socket.recv(&mut buffer) => {
                            match received {
                                Ok(length) => match cipher.open(&buffer[..length]) {
                                    Ok(packet) if packet.flags & FLAG_PROBE == 0 => {
                                        last_authenticated_media = Instant::now();
                                        received_media = true;
                                        task_stats.authenticated_packets.fetch_add(1, Ordering::Relaxed);
                                        if jitter.push(packet) {
                                            for _ in 0..MAX_PLAYOUT_FRAMES_PER_DATAGRAM {
                                                let Some(packet) = jitter.pop_ready() else {
                                                    break;
                                                };
                                                if packet.is_none() {
                                                    task_stats.concealed_packets.fetch_add(1, Ordering::Relaxed);
                                                }
                                                match concealer.decode(packet.as_ref().map(|packet| packet.payload.as_slice())) {
                                                    Ok(pcm) => player.push_48k_stereo(&pcm),
                                                    Err(error) => tracing::debug!(%error, "rejected PCM audio frame"),
                                                }
                                            }
                                        } else {
                                            task_stats.late_packets.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(error) => {
                                        task_stats.rejected_packets.fetch_add(1, Ordering::Relaxed);
                                        tracing::debug!(%error, "rejected audio datagram");
                                    }
                                },
                                Err(error) => {
                                    tracing::warn!(%error, "audio UDP receive failed");
                                    break format!("audio UDP receive failed: {error}");
                                }
                            }
                        }
                        _ = probe_retry.tick(), if !received_media => {
                            if let Err(error) = socket.send(&probe).await {
                                tracing::warn!(%error, "audio UDP probe retry failed");
                                break format!("audio UDP probe retry failed: {error}");
                            }
                        }
                        _ = media_watchdog.tick() => {
                            if !expecting_media && task_linux_streaming.load(Ordering::Acquire) {
                                expecting_media = true;
                                last_authenticated_media = Instant::now();
                            }
                            if expecting_media && last_authenticated_media.elapsed() > Duration::from_secs(2) {
                                tracing::warn!("Linux audio media timed out");
                                break "no authenticated Linux UDP audio received for 2 seconds after streaming started".to_string();
                            }
                            if !expecting_media && receiver_started.elapsed() > Duration::from_secs(8) {
                                tracing::warn!("Linux audio startup timed out");
                                break "Linux did not start audio media within 8 seconds".to_string();
                            }
                        }
                        changed = output_change_rx.recv() => {
                            if let Some(name) = changed
                                && name != player.output_name()
                            {
                                match AudioPlayer::open_default(task_stats.clone()) {
                                    Ok(updated) => {
                                        tracing::info!(previous = %player.output_name(), current = %updated.output_name(), "followed Windows default audio output change");
                                        player = updated;
                                    }
                                    Err(error) => tracing::warn!(%error, "failed to follow Windows default audio output change"),
                                }
                            }
                        }
                    }
                };
                output_monitor.abort();
                reason
            });
            Ok(Self {
                task: Some(task),
                linux_streaming,
                stats,
            })
        }

        pub fn is_finished(&self) -> bool {
            self.task.as_ref().is_none_or(|task| task.is_finished())
        }

        pub fn mark_linux_streaming(&self) {
            self.linux_streaming.store(true, Ordering::Release);
        }

        pub fn stats(&self) -> WindowsAudioStats {
            let snapshot = self.stats.snapshot();
            WindowsAudioStats {
                authenticated_packets: snapshot.authenticated_packets,
                rejected_packets: snapshot.rejected_packets,
                late_packets: snapshot.late_packets,
                concealed_packets: snapshot.concealed_packets,
                output_underruns: snapshot.output_underruns,
                dropped_output_frames: snapshot.dropped_output_frames,
                queued_output_ms: snapshot.queued_output_ms,
            }
        }

        pub async fn failure_reason(mut self) -> String {
            let Some(task) = self.task.take() else {
                return "Windows audio receiver stopped without a result".to_string();
            };
            match task.await {
                Ok(reason) => reason,
                Err(error) => format!("Windows audio receiver task failed: {error}"),
            }
        }
    }

    pub fn play_test_tone() -> Result<()> {
        edge_audio_output::play_test_tone()
    }

    #[cfg(test)]
    mod tests {
        use edge_audio::{CHANNELS, PcmCodec, PcmConcealer, SAMPLES_PER_CHANNEL};

        #[test]
        fn packet_loss_is_faded_out_and_recovery_is_faded_in() {
            let pcm = vec![0.5; SAMPLES_PER_CHANNEL * CHANNELS];
            let encoded = PcmCodec::encode(&pcm).unwrap();
            let mut concealer = PcmConcealer::default();
            let decoded = concealer.decode(Some(&encoded)).unwrap();
            assert!(decoded[decoded.len() - 1] > 0.49);

            let concealed = concealer.decode(None).unwrap();
            assert!(concealed[0] > concealed[concealed.len() - CHANNELS]);
            assert_eq!(concealed[concealed.len() - 1], 0.0);

            let recovered = concealer.decode(Some(&encoded)).unwrap();
            assert!(recovered[0] < 0.02);
            assert!(recovered[(48 - 1) * CHANNELS] > 0.49);
        }
    }
}

#[cfg(windows)]
pub use implementation::*;
