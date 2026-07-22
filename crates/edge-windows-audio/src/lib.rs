#[cfg(windows)]
mod implementation {
    use std::{
        collections::VecDeque,
        net::SocketAddr,
        sync::mpsc::{self, SyncSender, TryRecvError},
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

    const PLAYBACK_QUEUE_CHUNKS: usize = 32;

    pub struct AudioPlayer {
        sender: SyncSender<Vec<f32>>,
        _stream: Stream,
        output_name: String,
        output_rate: u32,
        output_channels: usize,
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
            let (sender, receiver) = mpsc::sync_channel(PLAYBACK_QUEUE_CHUNKS);
            let stream = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(&device, config, receiver)?,
                SampleFormat::I16 => build_stream::<i16>(&device, config, receiver)?,
                SampleFormat::U16 => build_stream::<u16>(&device, config, receiver)?,
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
                output_rate,
                output_channels,
            })
        }

        fn default_output_name() -> Result<String> {
            let device = cpal::default_host()
                .default_output_device()
                .context("Windows has no default audio output")?;
            Ok(device.to_string())
        }

        pub fn push_48k_stereo(&self, pcm: &[f32]) {
            let converted = convert_output(pcm, self.output_rate, self.output_channels);
            let _ = self.sender.try_send(converted);
        }
    }

    fn build_stream<T>(
        device: &Device,
        config: StreamConfig,
        receiver: mpsc::Receiver<Vec<f32>>,
    ) -> Result<Stream>
    where
        T: SizedSample + Sample + FromSample<f32>,
    {
        let mut queued = VecDeque::with_capacity(SAMPLE_RATE as usize * 2);
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
                    for sample in output {
                        *sample = T::from_sample(queued.pop_front().unwrap_or(0.0));
                    }
                },
                |error| tracing::warn!(%error, "Windows audio output error"),
                None,
            )
            .context("failed to build Windows output stream")
    }

    fn convert_output(input: &[f32], output_rate: u32, output_channels: usize) -> Vec<f32> {
        if input.is_empty() || output_channels == 0 {
            return Vec::new();
        }
        let input_frames = input.len() / CHANNELS;
        let output_frames =
            ((input_frames as u64 * output_rate as u64) / SAMPLE_RATE as u64) as usize;
        let mut output = Vec::with_capacity(output_frames * output_channels);
        for output_frame in 0..output_frames {
            let source = output_frame as f64 * SAMPLE_RATE as f64 / output_rate as f64;
            let left_index = source.floor() as usize;
            let right_index = (left_index + 1).min(input_frames.saturating_sub(1));
            let fraction = (source - left_index as f64) as f32;
            let left = [input[left_index * 2], input[left_index * 2 + 1]];
            let right = [input[right_index * 2], input[right_index * 2 + 1]];
            let stereo = [
                left[0] + (right[0] - left[0]) * fraction,
                left[1] + (right[1] - left[1]) * fraction,
            ];
            for channel in 0..output_channels {
                output.push(match channel {
                    0 => stereo[0],
                    1 => stereo[1],
                    _ => (stereo[0] + stereo[1]) * 0.5,
                });
            }
        }
        output
    }

    pub struct WindowsAudioReceiver {
        task: JoinHandle<()>,
    }

    impl Drop for WindowsAudioReceiver {
        fn drop(&mut self) {
            self.task.abort();
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

            let task = tokio::spawn(async move {
                let mut player = player;
                let mut jitter = JitterBuffer::new(jitter_target_ms);
                let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
                let mut playback = time::interval(Duration::from_millis(FRAME_MS as u64));
                playback.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                let mut media_watchdog = time::interval(Duration::from_millis(500));
                let mut output_watchdog = time::interval(Duration::from_secs(1));
                let mut last_authenticated_media = Instant::now();
                loop {
                    tokio::select! {
                        received = socket.recv(&mut buffer) => {
                            match received {
                                Ok(length) => match cipher.open(&buffer[..length]) {
                                    Ok(packet) if packet.flags & FLAG_PROBE == 0 => {
                                        last_authenticated_media = Instant::now();
                                        jitter.push(packet);
                                    }
                                    Ok(_) => {}
                                    Err(error) => tracing::debug!(%error, "rejected audio datagram"),
                                },
                                Err(error) => {
                                    tracing::warn!(%error, "audio UDP receive failed");
                                    break;
                                }
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
                            if last_authenticated_media.elapsed() > Duration::from_secs(2) {
                                tracing::warn!("Linux audio media timed out");
                                break;
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
            Ok(Self { task })
        }

        pub fn is_finished(&self) -> bool {
            self.task.is_finished()
        }

        pub fn stop(self) {
            drop(self);
        }
    }

    pub fn play_test_tone() -> Result<()> {
        let player = AudioPlayer::open_default()?;
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
            let output = convert_output(&input, 44_100, 2);
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
