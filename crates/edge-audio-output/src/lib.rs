//! Clock-driven audio playback shared by every platform.
//!
//! Decoded 48 kHz stereo frames are pushed into a lock-free ring. The audio
//! device pulls from that ring in its own callback, so playback is paced by
//! the output clock rather than by packet arrival. The ring absorbs network
//! bursts and gaps, and a small playback-rate correction keeps it near its
//! target depth so sender and receiver clock drift never accumulates.

use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use edge_audio::{CHANNELS, SAMPLE_RATE};

#[cfg(any(windows, target_os = "linux"))]
mod device;
#[cfg(any(windows, target_os = "linux"))]
pub use device::{AudioPlayer, play_test_tone};

/// Silence the device plays before starting, and after an underrun.
pub const OUTPUT_PREBUFFER_MS: u32 = 30;
/// Queue depth the rate correction steers towards.
pub const OUTPUT_TARGET_MS: u32 = 60;
/// Hard ring capacity; frames beyond it are dropped and counted.
pub const OUTPUT_QUEUE_LIMIT_MS: u32 = 180;
/// Largest playback-rate adjustment, ±0.5 %.
const MAX_CLOCK_CORRECTION: f64 = 0.005;
/// Full correction is reached when the smoothed queue is 25 % off target.
const CLOCK_CORRECTION_GAIN: f64 = 4.0;
/// Smoothing applied to the queue depth once per pushed 5 ms frame. A time
/// constant of about half a second ignores burst-to-burst swings, which would
/// otherwise wobble the pitch, while staying far faster than the correction
/// loop itself, so the loop stays well damped.
const QUEUE_SMOOTHING: f64 = 0.01;
/// Fade length for underruns and restarts: 1 ms at 48 kHz.
const FADE_FRAMES: usize = 48;

/// Single-producer, single-consumer ring of interleaved output samples.
pub struct AudioRing {
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
    pub fn new(capacity: usize) -> Self {
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

    pub fn len(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
            .min(self.capacity)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pushes whole frames of `alignment` samples and returns how many samples
    /// were accepted.
    pub fn push_slice_aligned(&self, input: &[f32], alignment: usize) -> usize {
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

    pub fn pop(&self) -> Option<f32> {
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

/// Counters shared between the network task, the player, and the callback.
#[derive(Default)]
pub struct PlaybackStats {
    pub authenticated_packets: AtomicU64,
    pub rejected_packets: AtomicU64,
    pub late_packets: AtomicU64,
    pub concealed_packets: AtomicU64,
    pub output_underruns: AtomicU64,
    pub dropped_output_frames: AtomicU64,
    pub queued_output_samples: AtomicUsize,
    pub output_samples_per_ms: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackStatsSnapshot {
    pub authenticated_packets: u64,
    pub rejected_packets: u64,
    pub late_packets: u64,
    pub concealed_packets: u64,
    pub output_underruns: u64,
    pub dropped_output_frames: u64,
    pub queued_output_ms: usize,
}

impl PlaybackStats {
    pub fn snapshot(&self) -> PlaybackStatsSnapshot {
        let samples_per_ms = self.output_samples_per_ms.load(Ordering::Relaxed).max(1);
        PlaybackStatsSnapshot {
            authenticated_packets: self.authenticated_packets.load(Ordering::Relaxed),
            rejected_packets: self.rejected_packets.load(Ordering::Relaxed),
            late_packets: self.late_packets.load(Ordering::Relaxed),
            concealed_packets: self.concealed_packets.load(Ordering::Relaxed),
            output_underruns: self.output_underruns.load(Ordering::Relaxed),
            dropped_output_frames: self.dropped_output_frames.load(Ordering::Relaxed),
            queued_output_ms: self.queued_output_samples.load(Ordering::Relaxed) / samples_per_ms,
        }
    }
}

pub fn duration_samples(rate: u32, channels: usize, duration_ms: u32) -> usize {
    ((rate as u64 * channels as u64 * duration_ms as u64) / 1_000) as usize
}

/// Resamples 48 kHz stereo to the device rate and channel count, with a
/// variable rate scale used for clock-drift correction.
pub struct OutputConverter {
    output_rate: u32,
    output_channels: usize,
    source_position: f64,
    input_frames: VecDeque<[f32; CHANNELS]>,
}

impl OutputConverter {
    pub fn new(output_rate: u32, output_channels: usize) -> Self {
        Self {
            output_rate,
            output_channels,
            source_position: 0.0,
            input_frames: VecDeque::new(),
        }
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    pub fn convert(&mut self, input: &[f32], rate_scale: f64) -> Vec<f32> {
        if input.is_empty() || self.output_channels == 0 || self.output_rate == 0 {
            return Vec::new();
        }
        self.input_frames
            .extend(input.as_chunks::<CHANNELS>().0.iter().copied());
        let step = SAMPLE_RATE as f64 / (self.output_rate as f64 * rate_scale);
        let estimated_frames = ((self.input_frames.len() as f64 - self.source_position).max(0.0)
            / step)
            .ceil() as usize;
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

/// Playback-rate scale that steers the smoothed queue towards its target.
/// Values above 1.0 stretch the input to refill a short queue.
pub fn clock_correction(queued_samples: f64, target_samples: usize) -> f64 {
    let target = target_samples.max(1) as f64;
    let error = (target - queued_samples) / target;
    1.0 + (error * CLOCK_CORRECTION_GAIN * MAX_CLOCK_CORRECTION)
        .clamp(-MAX_CLOCK_CORRECTION, MAX_CLOCK_CORRECTION)
}

/// Exponentially smoothed queue depth used by the drift correction.
#[derive(Debug, Default)]
pub struct QueueAverage {
    value: Option<f64>,
}

impl QueueAverage {
    pub fn update(&mut self, queued_samples: usize) -> f64 {
        let queued = queued_samples as f64;
        let value = match self.value {
            Some(value) => value + QUEUE_SMOOTHING * (queued - value),
            None => queued,
        };
        self.value = Some(value);
        value
    }
}

/// Output-callback state: prebuffering, underrun detection, and short fades so
/// starting, stopping, and running dry never produce a hard step.
pub struct PlayoutState {
    ring: Arc<AudioRing>,
    stats: Arc<PlaybackStats>,
    channels: usize,
    prebuffer_samples: usize,
    playing: bool,
    gain: f32,
    last_frame: Vec<f32>,
}

impl PlayoutState {
    pub fn new(
        ring: Arc<AudioRing>,
        stats: Arc<PlaybackStats>,
        channels: usize,
        prebuffer_samples: usize,
    ) -> Self {
        let channels = channels.max(1);
        Self {
            ring,
            stats,
            channels,
            prebuffer_samples,
            playing: false,
            gain: 0.0,
            last_frame: vec![0.0; channels],
        }
    }

    /// Fills interleaved output samples from the ring.
    pub fn render(&mut self, output: &mut [f32]) {
        let step = 1.0 / FADE_FRAMES as f32;
        for frame in output.chunks_mut(self.channels) {
            if !self.playing && self.ring.len() >= self.prebuffer_samples.max(self.channels) {
                self.playing = true;
            }
            if self.playing && self.ring.len() >= self.channels {
                for sample in &mut self.last_frame {
                    *sample = self.ring.pop().unwrap_or(0.0);
                }
                self.gain = (self.gain + step).min(1.0);
            } else {
                if self.playing {
                    self.playing = false;
                    self.stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                }
                // Decay the last real frame instead of jumping to silence.
                self.gain = (self.gain - step).max(0.0);
            }
            for (out, sample) in frame.iter_mut().zip(&self.last_frame) {
                *out = sample * self.gain;
            }
        }
        self.stats
            .queued_output_samples
            .store(self.ring.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edge_audio::SAMPLES_PER_CHANNEL;

    #[test]
    fn resampler_preserves_duration_and_channels() {
        let input = vec![0.25; 480 * 2];
        let mut converter = OutputConverter::new(44_100, 2);
        let mut output = Vec::new();
        for frame in input.as_chunks::<{ SAMPLES_PER_CHANNEL * CHANNELS }>().0 {
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
    fn clock_correction_is_bounded_and_steers_to_target() {
        assert_eq!(clock_correction(100.0, 100), 1.0);
        assert!(clock_correction(90.0, 100) > 1.0);
        assert!(clock_correction(110.0, 100) < 1.0);
        assert!((clock_correction(75.0, 100) - (1.0 + MAX_CLOCK_CORRECTION)).abs() < 1e-12);
        assert!((clock_correction(0.0, 100) - 1.0).abs() <= MAX_CLOCK_CORRECTION + f64::EPSILON);
        assert!(
            (clock_correction(10_000.0, 100) - 1.0).abs() <= MAX_CLOCK_CORRECTION + f64::EPSILON
        );
    }

    #[test]
    fn queue_average_smooths_burst_swings() {
        let mut average = QueueAverage::default();
        assert_eq!(average.update(1_000), 1_000.0);
        let mut last = 0.0;
        for step in 0..400 {
            // Alternate between a full and an empty burst position.
            last = average.update(if step % 4 == 0 { 2_000 } else { 667 });
        }
        assert!(
            (last - 1_000.0).abs() < 100.0,
            "average stays near the mean: {last}"
        );
    }

    #[test]
    fn playout_prebuffers_fades_in_and_fades_out_on_underrun() {
        let ring = Arc::new(AudioRing::new(4_096));
        let stats = Arc::new(PlaybackStats::default());
        let mut playout = PlayoutState::new(ring.clone(), stats.clone(), 2, 200);

        let mut output = vec![1.0; 40];
        playout.render(&mut output);
        assert!(
            output.iter().all(|sample| *sample == 0.0),
            "silent before prebuffer"
        );

        ring.push_slice_aligned(&vec![0.5; 400], 2);
        let mut output = vec![0.0; 400];
        playout.render(&mut output);
        assert!(
            output[0] > 0.0 && output[0] < 0.05,
            "fade-in starts quietly"
        );
        assert!(
            (output[FADE_FRAMES * 2] - 0.5).abs() < 1e-6,
            "reaches full level"
        );
        assert_eq!(stats.output_underruns.load(Ordering::Relaxed), 0);

        let mut output = vec![1.0; 200];
        playout.render(&mut output);
        assert_eq!(stats.output_underruns.load(Ordering::Relaxed), 1);
        assert!(
            output[0] > 0.45 && output[0] < 0.5,
            "underrun decays from the last frame"
        );
        assert!(output.windows(2).all(|pair| pair[1] <= pair[0] + 1e-6));
        assert_eq!(*output.last().unwrap(), 0.0, "fully silent after the fade");
    }
}
