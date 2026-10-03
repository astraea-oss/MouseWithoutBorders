//! CPAL output device wrapper used by both the Windows and Linux receivers.

use std::{
    sync::{Arc, atomic::Ordering, mpsc as std_mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use edge_audio::{FRAME_MS, SAMPLE_RATE, SAMPLES_PER_CHANNEL};

use crate::{
    AudioRing, OUTPUT_PREBUFFER_MS, OUTPUT_QUEUE_LIMIT_MS, OUTPUT_TARGET_MS, OutputConverter,
    PlaybackStats, PlayoutState, QueueAverage, clock_correction, duration_samples,
};

/// Overrides CPAL's host choice, for example `ALSA` or `PipeWire`.
pub const AUDIO_HOST_ENV: &str = "EDGE_KVM_AUDIO_HOST";

/// Plays 48 kHz stereo PCM on the default output device.
pub struct AudioPlayer {
    ring: Arc<AudioRing>,
    converter: OutputConverter,
    stats: Arc<PlaybackStats>,
    target_queue_samples: usize,
    queue_average: QueueAverage,
    output_name: String,
    _stream: StreamThread,
}

struct OpenedStream {
    ring: Arc<AudioRing>,
    output_name: String,
    host_name: &'static str,
    output_rate: u32,
    output_channels: usize,
}

/// Owns the CPAL stream on a dedicated thread.
///
/// Some CPAL backends do not allow a `Stream` to move between threads. Keeping
/// it on its own thread lets `AudioPlayer` move freely between async tasks on
/// every backend. Dropping the guard closes the stop channel, which drops the
/// stream on its own thread.
struct StreamThread {
    _stop: std_mpsc::Sender<()>,
}

impl AudioPlayer {
    pub fn open_default(stats: Arc<PlaybackStats>) -> Result<Self> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<OpenedStream>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let thread_stats = stats.clone();
        thread::Builder::new()
            .name("edge-kvm-audio-output".to_string())
            .spawn(move || {
                let stream = match open_stream(thread_stats) {
                    Ok((stream, opened)) => {
                        let _ = ready_tx.send(Ok(opened));
                        stream
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                // Blocks until the player is dropped and the sender closes.
                let _ = stop_rx.recv();
                drop(stream);
            })
            .context("failed to start the audio output thread")?;
        let opened = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("audio output did not open within 5 seconds")??;
        tracing::info!(
            host = opened.host_name,
            output_rate = opened.output_rate,
            output_channels = opened.output_channels,
            output_name = %opened.output_name,
            "opened audio output"
        );
        Ok(Self {
            ring: opened.ring,
            converter: OutputConverter::new(opened.output_rate, opened.output_channels),
            stats,
            target_queue_samples: duration_samples(
                opened.output_rate,
                opened.output_channels,
                OUTPUT_TARGET_MS,
            ),
            queue_average: QueueAverage::default(),
            output_name: opened.output_name,
            _stream: StreamThread { _stop: stop_tx },
        })
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn default_output_name() -> Result<String> {
        let device = select_host()
            .default_output_device()
            .context("no default audio output")?;
        Ok(device.to_string())
    }

    /// Queues one decoded frame, nudging the playback rate by up to ±0.5 % so
    /// the queue stays near its target and clock drift cannot accumulate.
    pub fn push_48k_stereo(&mut self, pcm: &[f32]) {
        let queued = self.queue_average.update(self.ring.len());
        let rate_scale = clock_correction(queued, self.target_queue_samples);
        let converted = self.converter.convert(pcm, rate_scale);
        let channels = self.converter.output_channels().max(1);
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

fn select_host() -> cpal::Host {
    if let Ok(requested) = std::env::var(AUDIO_HOST_ENV) {
        let requested = requested.trim();
        let host = cpal::available_hosts()
            .into_iter()
            .find(|id| id.name().eq_ignore_ascii_case(requested))
            .and_then(|id| cpal::host_from_id(id).ok());
        match host {
            Some(host) => return host,
            None => tracing::warn!(
                requested,
                "requested audio host is unavailable; using the default host"
            ),
        }
    }
    cpal::default_host()
}

fn open_stream(stats: Arc<PlaybackStats>) -> Result<(Stream, OpenedStream)> {
    let host = select_host();
    let host_name = host.id().name();
    let device = host
        .default_output_device()
        .context("no default audio output")?;
    let output_name = device.to_string();
    let supported = device
        .default_output_config()
        .context("failed to query the default audio output format")?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let output_rate = config.sample_rate;
    let output_channels = config.channels as usize;
    let ring = Arc::new(AudioRing::new(duration_samples(
        output_rate,
        output_channels,
        OUTPUT_QUEUE_LIMIT_MS,
    )));
    stats.output_samples_per_ms.store(
        duration_samples(output_rate, output_channels, 1).max(1),
        Ordering::Relaxed,
    );
    let playout = PlayoutState::new(
        ring.clone(),
        stats,
        output_channels,
        duration_samples(output_rate, output_channels, OUTPUT_PREBUFFER_MS),
    );
    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, config, playout)?,
        SampleFormat::I16 => build_stream::<i16>(&device, config, playout)?,
        SampleFormat::U16 => build_stream::<u16>(&device, config, playout)?,
        SampleFormat::I32 => build_stream::<i32>(&device, config, playout)?,
        format => anyhow::bail!("unsupported audio output sample format: {format}"),
    };
    stream.play().context("failed to start audio output")?;
    Ok((
        stream,
        OpenedStream {
            ring,
            output_name,
            host_name,
            output_rate,
            output_channels,
        },
    ))
}

fn build_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut playout: PlayoutState,
) -> Result<Stream>
where
    T: SizedSample + Sample + FromSample<f32>,
{
    let mut scratch = Vec::<f32>::new();
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| {
                if scratch.len() < output.len() {
                    scratch.resize(output.len(), 0.0);
                }
                let rendered = &mut scratch[..output.len()];
                playout.render(rendered);
                for (sample, value) in output.iter_mut().zip(rendered.iter()) {
                    *sample = T::from_sample(*value);
                }
            },
            |error| tracing::warn!(%error, "audio output error"),
            None,
        )
        .context("failed to build audio output stream")
}

/// Plays a one-second 440 Hz tone through the shared playback path.
pub fn play_test_tone() -> Result<()> {
    let stats = Arc::new(PlaybackStats::default());
    let mut player = AudioPlayer::open_default(stats.clone())?;
    for frame in 0..200 {
        let mut pcm = Vec::with_capacity(SAMPLES_PER_CHANNEL * 2);
        for sample in 0..SAMPLES_PER_CHANNEL {
            let t = (frame * SAMPLES_PER_CHANNEL + sample) as f32 / SAMPLE_RATE as f32;
            let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.18;
            pcm.extend_from_slice(&[value, value]);
        }
        player.push_48k_stereo(&pcm);
        thread::sleep(Duration::from_millis(FRAME_MS as u64));
    }
    // Let the queued tail play out before the stream closes.
    thread::sleep(Duration::from_millis(u64::from(OUTPUT_TARGET_MS) * 2));
    let snapshot = stats.snapshot();
    // Draining the final queue counts as one underrun; more indicate glitches.
    tracing::info!(
        output_underruns = snapshot.output_underruns,
        dropped_output_frames = snapshot.dropped_output_frames,
        queued_output_ms = snapshot.queued_output_ms,
        "audio test tone finished"
    );
    anyhow::ensure!(
        snapshot.output_underruns >= 1 && snapshot.queued_output_ms == 0,
        "audio output did not consume the test tone"
    );
    Ok(())
}
