#[cfg(windows)]
mod implementation {
    use std::{
        collections::VecDeque,
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, SyncSender, TryRecvError},
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

    const PLAYBACK_QUEUE_CHUNKS: usize = 64;
    const OUTPUT_PREBUFFER_MS: u32 = 30;
    const OUTPUT_QUEUE_LIMIT_MS: u32 = 200;

    pub struct AudioPlayer {
        sender: SyncSender<Vec<f32>>,
        _stream: Stream,
        output_name: String,
        converter: OutputConverter,
    }

    impl AudioPlayer {
        pub fn open_default() -> Result<Self> {
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
            let (sender, receiver) = mpsc::sync_channel(PLAYBACK_QUEUE_CHUNKS);
            let stream = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(
                    &device,
                    config,
                    receiver,
                    prebuffer_samples,
                    queue_limit_samples,
                )?,
                SampleFormat::I16 => build_stream::<i16>(
                    &device,
                    config,
                    receiver,
                    prebuffer_samples,
                    queue_limit_samples,
                )?,
                SampleFormat::U16 => build_stream::<u16>(
                    &device,
                    config,
                    receiver,
                    prebuffer_samples,
                    queue_limit_samples,
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
                sender,
                _stream: stream,
                output_name,
                converter: OutputConverter::new(output_rate, output_channels),
            })
        }

        fn default_output_name() -> Result<String> {
            let device = cpal::default_host()
                .default_output_device()
                .context("Windows has no default audio output")?;
            Ok(device.to_string())
        }

        pub fn push_48k_stereo(&mut self, pcm: &[f32]) {
            let converted = self.converter.convert(pcm);
            let _ = self.sender.try_send(converted);
        }
    }

    fn build_stream<T>(
        device: &Device,
        config: StreamConfig,
        receiver: mpsc::Receiver<Vec<f32>>,
        prebuffer_samples: usize,
        queue_limit_samples: usize,
    ) -> Result<Stream>
    where
        T: SizedSample + Sample + FromSample<f32>,
    {
        let mut queued = VecDeque::with_capacity(SAMPLE_RATE as usize * 2);
        let mut playback_started = false;
        device
            .build_output_stream(
                config,
                move |output: &mut [T], _| {
                    loop {
                        match receiver.try_recv() {
                            Ok(chunk) => queued.extend(chunk),
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }
                    while queued.len() > queue_limit_samples {
                        queued.pop_front();
                    }
                    if !playback_started && queued.len() >= prebuffer_samples {
                        playback_started = true;
                    }
                    for sample in output {
                        let value = if playback_started {
                            match queued.pop_front() {
                                Some(value) => value,
                                None => {
                                    playback_started = false;
                                    0.0
                                }
                            }
                        } else {
                            0.0
                        };
                        *sample = T::from_sample(value);
                    }
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

        fn convert(&mut self, input: &[f32]) -> Vec<f32> {
            if input.is_empty() || self.output_channels == 0 || self.output_rate == 0 {
                return Vec::new();
            }
            self.input_frames.extend(
                input
                    .chunks_exact(CHANNELS)
                    .map(|frame| [frame[0], frame[1]]),
            );
            let step = SAMPLE_RATE as f64 / self.output_rate as f64;
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

    pub struct WindowsAudioReceiver {
        task: Option<JoinHandle<String>>,
        linux_streaming: Arc<AtomicBool>,
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
            let player = AudioPlayer::open_default()?;
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
            let task = tokio::spawn(async move {
                let mut player = player;
                let mut jitter = JitterBuffer::new(jitter_target_ms);
                let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
                let mut playback = time::interval(Duration::from_millis(FRAME_MS as u64));
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
                                        jitter.push(packet);
                                    }
                                    Ok(_) => {}
                                    Err(error) => tracing::debug!(%error, "rejected audio datagram"),
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
                                match PcmCodec::decode(packet.as_ref().map(|packet| packet.payload.as_slice())) {
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
                                    match AudioPlayer::open_default() {
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
            })
        }

        pub fn is_finished(&self) -> bool {
            self.task.as_ref().is_none_or(|task| task.is_finished())
        }

        pub fn mark_linux_streaming(&self) {
            self.linux_streaming.store(true, Ordering::Release);
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
                output.extend(converter.convert(frame));
            }
            assert_eq!(output.len(), 441 * 2);
            assert!(
                output
                    .iter()
                    .all(|sample| (*sample - 0.25).abs() < f32::EPSILON)
            );
        }
    }
}

#[cfg(windows)]
pub use implementation::*;
