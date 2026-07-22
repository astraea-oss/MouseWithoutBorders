#[cfg(windows)]
mod implementation {
    use std::{
        cell::UnsafeCell,
        collections::VecDeque,
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result};
    use cpal::{
        Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };
    use edge_audio::{
        AudioPacket, CHANNELS, FLAG_PROBE, FRAME_MS, JitterBuffer, MAX_DATAGRAM_BYTES,
        PacketCipher, PcmCodec, SAMPLE_RATE, SAMPLES_PER_CHANNEL, SessionSecrets,
    };
    use tokio::{net::UdpSocket, task::JoinHandle, time};

    const OUTPUT_PREBUFFER_MS: u32 = 30;
    const OUTPUT_TARGET_MS: u32 = 60;
    const OUTPUT_QUEUE_LIMIT_MS: u32 = 180;
    const MAX_CLOCK_CORRECTION: f64 = 0.005;

    struct AudioRing {
        samples: Box<[UnsafeCell<f32>]>,
        capacity: usize,
        read: AtomicUsize,
        write: AtomicUsize,
    }

    // AudioRing has exactly one producer and one consumer. The producer writes
    // only slots outside the consumer's readable range and publishes them with
    // Release; the callback reads only published slots after an Acquire load.
    unsafe impl Sync for AudioRing {}

    impl AudioRing {
        fn new(capacity: usize) -> Self {
            let capacity = capacity.max(2);
            let samples = (0..capacity)
                .map(|_| UnsafeCell::new(0.0))
                .collect::<Vec<_>>()
                .into_boxed_slice();
            Self {
                samples,
                capacity,
                read: AtomicUsize::new(0),
                write: AtomicUsize::new(0),
            }
        }

        fn len(&self) -> usize {
            self.write
                .load(Ordering::Acquire)
                .wrapping_sub(self.read.load(Ordering::Acquire))
                .min(self.capacity)
        }

        fn push_slice_aligned(&self, input: &[f32], alignment: usize) -> usize {
            let write = self.write.load(Ordering::Relaxed);
            let read = self.read.load(Ordering::Acquire);
            let available = self.capacity.saturating_sub(write.wrapping_sub(read));
            let alignment = alignment.max(1);
            let count = input.len().min(available) / alignment * alignment;
            for (offset, sample) in input[..count].iter().enumerate() {
                let index = write.wrapping_add(offset) % self.capacity;
                // SAFETY: this is the single producer, and free-space accounting
                // guarantees the consumer cannot currently access this slot.
                unsafe { *self.samples[index].get() = *sample };
            }
            self.write
                .store(write.wrapping_add(count), Ordering::Release);
            count
        }

        fn pop(&self) -> Option<f32> {
            let read = self.read.load(Ordering::Relaxed);
            if read == self.write.load(Ordering::Acquire) {
                return None;
            }
            let index = read % self.capacity;
            // SAFETY: this is the single consumer, and the producer published
            // this slot before advancing write with Release ordering.
            let sample = unsafe { *self.samples[index].get() };
            self.read.store(read.wrapping_add(1), Ordering::Release);
            Some(sample)
        }
    }

    #[derive(Default)]
    struct PlaybackStats {
        authenticated_packets: AtomicU64,
        rejected_packets: AtomicU64,
        late_packets: AtomicU64,
        concealed_packets: AtomicU64,
        output_underruns: AtomicU64,
        dropped_output_frames: AtomicU64,
        queued_output_samples: AtomicUsize,
        output_samples_per_ms: AtomicUsize,
    }

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

    pub struct AudioPlayer {
        ring: Arc<AudioRing>,
        _stream: Stream,
        output_name: String,
        converter: OutputConverter,
        stats: Arc<PlaybackStats>,
        target_queue_samples: usize,
    }

    impl AudioPlayer {
        pub fn open_default() -> Result<Self> {
            Self::open_default_with_stats(Arc::new(PlaybackStats::default()))
        }

        fn open_default_with_stats(stats: Arc<PlaybackStats>) -> Result<Self> {
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .context("Windows has no default audio output")?;
            let output_name = device.to_string();
            let supported = device
                .default_output_config()
                .context("failed to query the default Windows audio format")?;
            let sample_format = supported.sample_format();
            let config: StreamConfig = supported.into();
            let output_rate = config.sample_rate;
            let output_channels = config.channels as usize;
            let prebuffer_samples =
                duration_samples(output_rate, output_channels, OUTPUT_PREBUFFER_MS);
            let queue_limit_samples =
                duration_samples(output_rate, output_channels, OUTPUT_QUEUE_LIMIT_MS);
            let target_queue_samples =
                duration_samples(output_rate, output_channels, OUTPUT_TARGET_MS);
            let ring = Arc::new(AudioRing::new(queue_limit_samples));
            stats.output_samples_per_ms.store(
                duration_samples(output_rate, output_channels, 1).max(1),
                Ordering::Relaxed,
            );
            let stream = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(
                    &device,
                    config,
                    ring.clone(),
                    prebuffer_samples,
                    stats.clone(),
                )?,
                SampleFormat::I16 => build_stream::<i16>(
                    &device,
                    config,
                    ring.clone(),
                    prebuffer_samples,
                    stats.clone(),
                )?,
                SampleFormat::U16 => build_stream::<u16>(
                    &device,
                    config,
                    ring.clone(),
                    prebuffer_samples,
                    stats.clone(),
                )?,
                format => anyhow::bail!("unsupported Windows output sample format: {format}"),
            };
            stream
                .play()
                .context("failed to start Windows audio output")?;
            tracing::info!(
                output_rate,
                output_channels,
                output_name,
                "opened default Windows audio output"
            );
            Ok(Self {
                ring,
                _stream: stream,
                output_name,
                converter: OutputConverter::new(output_rate, output_channels),
                stats,
                target_queue_samples,
            })
        }

        fn default_output_name() -> Result<String> {
            let device = cpal::default_host()
                .default_output_device()
                .context("Windows has no default audio output")?;
            Ok(device.to_string())
        }

        pub fn push_48k_stereo(&mut self, pcm: &[f32]) {
            let queued = self.ring.len();
            let target = self.target_queue_samples.max(1);
            let error = (target as f64 - queued as f64) / target as f64;
            let correction =
                (error * MAX_CLOCK_CORRECTION).clamp(-MAX_CLOCK_CORRECTION, MAX_CLOCK_CORRECTION);
            let converted = self.converter.convert(pcm, 1.0 + correction);
            let channels = self.converter.output_channels.max(1);
            let pushed = self.ring.push_slice_aligned(&converted, channels);
            if pushed < converted.len() {
                self.stats.dropped_output_frames.fetch_add(
                    ((converted.len() - pushed) / channels) as u64,
                    Ordering::Relaxed,
                );
            }
            self.stats
                .queued_output_samples
                .store(self.ring.len(), Ordering::Relaxed);
        }
    }

    fn build_stream<T>(
        device: &Device,
        config: StreamConfig,
        ring: Arc<AudioRing>,
        prebuffer_samples: usize,
        stats: Arc<PlaybackStats>,
    ) -> Result<Stream>
    where
        T: SizedSample + Sample + FromSample<f32>,
    {
        let mut playback_started = false;
        device
            .build_output_stream(
                config,
                move |output: &mut [T], _| {
                    if !playback_started && ring.len() >= prebuffer_samples {
                        playback_started = true;
                    }
                    for sample in output {
                        let value = if playback_started {
                            match ring.pop() {
                                Some(value) => value,
                                None => {
                                    playback_started = false;
                                    stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                                    0.0
                                }
                            }
                        } else {
                            0.0
                        };
                        *sample = T::from_sample(value);
                    }
                    stats
                        .queued_output_samples
                        .store(ring.len(), Ordering::Relaxed);
                },
                |error| tracing::warn!(%error, "Windows audio output error"),
                None,
            )
            .context("failed to build Windows output stream")
    }

    fn duration_samples(rate: u32, channels: usize, duration_ms: u32) -> usize {
        ((rate as u64 * channels as u64 * duration_ms as u64) / 1_000) as usize
    }

    struct OutputConverter {
        output_rate: u32,
        output_channels: usize,
        source_position: f64,
        input_frames: VecDeque<[f32; CHANNELS]>,
    }

    impl OutputConverter {
        fn new(output_rate: u32, output_channels: usize) -> Self {
            Self {
                output_rate,
                output_channels,
                source_position: 0.0,
                input_frames: VecDeque::new(),
            }
        }

        fn convert(&mut self, input: &[f32], rate_scale: f64) -> Vec<f32> {
            if input.is_empty() || self.output_channels == 0 || self.output_rate == 0 {
                return Vec::new();
            }
            self.input_frames.extend(
                input
                    .chunks_exact(CHANNELS)
                    .map(|frame| [frame[0], frame[1]]),
            );
            let step = SAMPLE_RATE as f64 / (self.output_rate as f64 * rate_scale);
            let estimated_frames =
                ((self.input_frames.len() as f64 - self.source_position).max(0.0) / step).ceil()
                    as usize;
            let mut output = Vec::with_capacity(estimated_frames * self.output_channels);
            while self.source_position + 1.0 < self.input_frames.len() as f64 {
                let left_index = self.source_position.floor() as usize;
                let fraction = (self.source_position - left_index as f64) as f32;
                let left = self.input_frames[left_index];
                let right = self.input_frames[left_index + 1];
                let stereo = [
                    left[0] + (right[0] - left[0]) * fraction,
                    left[1] + (right[1] - left[1]) * fraction,
                ];
                for channel in 0..self.output_channels {
                    output.push(match channel {
                        0 => stereo[0],
                        1 => stereo[1],
                        _ => (stereo[0] + stereo[1]) * 0.5,
                    });
                }
                self.source_position += step;
            }
            let consumed = self.source_position.floor() as usize;
            self.input_frames.drain(..consumed);
            self.source_position -= consumed as f64;
            output
        }
    }

    #[derive(Default)]
    struct PcmConcealer {
        last_sample: [f32; CHANNELS],
        recovering: bool,
    }

    impl PcmConcealer {
        fn decode(&mut self, packet: Option<&[u8]>) -> edge_audio::Result<Vec<f32>> {
            let Some(packet) = packet else {
                self.recovering = true;
                let mut concealed = Vec::with_capacity(SAMPLES_PER_CHANNEL * CHANNELS);
                for frame in 0..SAMPLES_PER_CHANNEL {
                    let gain = 1.0 - (frame + 1) as f32 / SAMPLES_PER_CHANNEL as f32;
                    concealed.push(self.last_sample[0] * gain);
                    concealed.push(self.last_sample[1] * gain);
                }
                self.last_sample = [0.0; CHANNELS];
                return Ok(concealed);
            };

            let mut pcm = PcmCodec::decode(Some(packet))?;
            if self.recovering {
                let fade_frames = 48.min(SAMPLES_PER_CHANNEL);
                for frame in 0..fade_frames {
                    let gain = (frame + 1) as f32 / fade_frames as f32;
                    pcm[frame * CHANNELS] *= gain;
                    pcm[frame * CHANNELS + 1] *= gain;
                }
                self.recovering = false;
            }
            self.last_sample = [pcm[pcm.len() - CHANNELS], pcm[pcm.len() - CHANNELS + 1]];
            Ok(pcm)
        }
    }

    pub struct WindowsAudioReceiver {
        task: Option<JoinHandle<String>>,
        linux_streaming: Arc<AtomicBool>,
        stats: Arc<PlaybackStats>,
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
            let player = AudioPlayer::open_default_with_stats(stats.clone())?;
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
                let mut player = player;
                let mut jitter = JitterBuffer::new(jitter_target_ms);
                let mut concealer = PcmConcealer::default();
                let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
                let mut playback = time::interval(Duration::from_millis(FRAME_MS as u64));
                // Catching up missed wall-clock ticks would advance the jitter
                // sequence ahead of the live sender. Skip missed ticks and let
                // the bounded output queue's clock correction absorb drift.
                playback.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                let mut media_watchdog = time::interval(Duration::from_millis(500));
                let mut output_watchdog = time::interval(Duration::from_secs(1));
                let mut probe_retry = time::interval(Duration::from_millis(250));
                let receiver_started = Instant::now();
                let mut last_authenticated_media = Instant::now();
                let mut expecting_media = false;
                let mut received_media = false;
                loop {
                    tokio::select! {
                        received = socket.recv(&mut buffer) => {
                            match received {
                                Ok(length) => match cipher.open(&buffer[..length]) {
                                    Ok(packet) if packet.flags & FLAG_PROBE == 0 => {
                                        last_authenticated_media = Instant::now();
                                        received_media = true;
                                        task_stats.authenticated_packets.fetch_add(1, Ordering::Relaxed);
                                        if !jitter.push(packet) {
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
                        _ = playback.tick() => {
                            if let Some(packet) = jitter.pop() {
                                if packet.is_none() {
                                    task_stats.concealed_packets.fetch_add(1, Ordering::Relaxed);
                                }
                                match concealer.decode(packet.as_ref().map(|packet| packet.payload.as_slice())) {
                                    Ok(pcm) => player.push_48k_stereo(&pcm),
                                    Err(error) => tracing::debug!(%error, "rejected PCM audio frame"),
                                }
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
                        _ = output_watchdog.tick() => {
                            match AudioPlayer::default_output_name() {
                                Ok(name) if name != player.output_name => {
                                    match AudioPlayer::open_default_with_stats(task_stats.clone()) {
                                        Ok(updated) => {
                                            tracing::info!(previous = %player.output_name, current = %updated.output_name, "followed Windows default audio output change");
                                            player = updated;
                                        }
                                        Err(error) => tracing::warn!(%error, "failed to follow Windows default audio output change"),
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => tracing::warn!(%error, "failed to poll Windows default audio output"),
                            }
                        }
                    }
                }
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
            let samples_per_ms = self
                .stats
                .output_samples_per_ms
                .load(Ordering::Relaxed)
                .max(1);
            WindowsAudioStats {
                authenticated_packets: self.stats.authenticated_packets.load(Ordering::Relaxed),
                rejected_packets: self.stats.rejected_packets.load(Ordering::Relaxed),
                late_packets: self.stats.late_packets.load(Ordering::Relaxed),
                concealed_packets: self.stats.concealed_packets.load(Ordering::Relaxed),
                output_underruns: self.stats.output_underruns.load(Ordering::Relaxed),
                dropped_output_frames: self.stats.dropped_output_frames.load(Ordering::Relaxed),
                queued_output_ms: self.stats.queued_output_samples.load(Ordering::Relaxed)
                    / samples_per_ms,
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
        let mut player = AudioPlayer::open_default()?;
        for frame in 0..200 {
            let mut pcm = Vec::with_capacity(SAMPLES_PER_CHANNEL * 2);
            for sample in 0..SAMPLES_PER_CHANNEL {
                let t = (frame * SAMPLES_PER_CHANNEL + sample) as f32 / SAMPLE_RATE as f32;
                let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.18;
                pcm.extend_from_slice(&[value, value]);
            }
            player.push_48k_stereo(&pcm);
            std::thread::sleep(Duration::from_millis(FRAME_MS as u64));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn resampler_preserves_duration_and_channels() {
            let input = vec![0.25; 480 * 2];
            let mut converter = OutputConverter::new(44_100, 2);
            let mut output = Vec::new();
            for frame in input.chunks_exact(SAMPLES_PER_CHANNEL * CHANNELS) {
                output.extend(converter.convert(frame, 1.0));
            }
            assert_eq!(output.len(), 441 * 2);
            assert!(
                output
                    .iter()
                    .all(|sample| (*sample - 0.25).abs() < f32::EPSILON)
            );
        }

        #[test]
        fn audio_ring_is_bounded_and_preserves_order() {
            let ring = AudioRing::new(4);
            assert_eq!(ring.push_slice_aligned(&[1.0, 2.0, 3.0], 1), 3);
            assert_eq!(ring.pop(), Some(1.0));
            assert_eq!(ring.push_slice_aligned(&[4.0, 5.0, 6.0], 1), 2);
            assert_eq!(ring.len(), 4);
            assert_eq!(ring.pop(), Some(2.0));
            assert_eq!(ring.pop(), Some(3.0));
            assert_eq!(ring.pop(), Some(4.0));
            assert_eq!(ring.pop(), Some(5.0));
            assert_eq!(ring.pop(), None);

            let frame_ring = AudioRing::new(5);
            assert_eq!(
                frame_ring.push_slice_aligned(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2),
                4
            );
        }

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
