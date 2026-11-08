//! Video remote view (experimental, `phone-window-video-v1`): the selected
//! window's region is captured with wlr-screencopy into shared memory and
//! encoded to HEVC on NVENC through GStreamer. NVENC takes the captured BGRx
//! directly, so there is no software conversion. One access unit (Annex B,
//! parameter sets repeated on every keyframe) comes out per frame in, so
//! frames keep the JPEG view's frame-scoped metadata and input rules.
//!
//! Capture waits for damage: a still window produces a frame only every
//! [`KEEPALIVE`], which keeps the phone's view fresh enough to tap.
use anyhow::{Context, Result, ensure};
use edge_linux_input::{CapturedRegion, ScreenCopy};
use edge_protocol::{FocusRect, FocusedWindow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A still window still gets a frame this often.
pub(crate) const KEEPALIVE: Duration = Duration::from_millis(500);

/// Whether this computer can stream video: wlr-screencopy, and GStreamer with
/// NVENC HEVC.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        gst::init().is_ok()
            && ["nvh265enc", "h265parse", "appsrc", "appsink"]
                .iter()
                .all(|name| gst::ElementFactory::find(name).is_some())
            && ScreenCopy::connect().is_ok()
    })
}

/// Text favours sharp still content; Media favours motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Text,
    Media,
}

impl Mode {
    /// The shortest time between frames.
    pub fn interval(self) -> Duration {
        match self {
            Self::Text => Duration::from_millis(100),
            Self::Media => Duration::from_millis(16),
        }
    }

    /// Frames sent but not yet acked: one for Text, a few for Media, so the
    /// round trip doesn't cap the frame rate.
    pub fn in_flight(self) -> usize {
        match self {
            Self::Text => 1,
            Self::Media => 3,
        }
    }

    fn encoder_settings(self) -> &'static str {
        match self {
            Self::Text => "preset=p4 tune=low-latency rc-mode=vbr const-quality=20",
            Self::Media => {
                "preset=p1 tune=ultra-low-latency rc-mode=vbr const-quality=26 max-bitrate=20000"
            }
        }
    }
}

/// One NVENC session for a fixed size, pixel format and mode. Any change, or a
/// broken decoding chain on the phone, gets a new encoder, whose first frame is
/// a keyframe.
pub(crate) struct Encoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    key: (u32, u32, &'static str, Mode),
    frames: u64,
}

impl Encoder {
    pub fn new(width: u32, height: u32, format: &'static str, mode: Mode) -> Result<Self> {
        ensure!(available(), "This computer can't stream video");
        ensure!(
            width >= 144 && height >= 48,
            "The window is too small to stream ({width}x{height})"
        );
        ensure!(
            matches!(format, "BGRx" | "BGRA" | "RGBx" | "RGBA"),
            "Unsupported capture format {format}"
        );
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false \
               caps=video/x-raw,format={format},width={width},height={height},framerate=0/1 \
             ! nvh265enc {settings} zerolatency=true bframes=0 gop-size=-1 \
               repeat-sequence-header=true \
             ! h265parse config-interval=-1 \
             ! video/x-h265,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=2",
            settings = mode.encoder_settings(),
        );
        let pipeline = gst::parse::launch(&description)
            .context("Could not build the video encoder")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Video encoder is not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("Video encoder has no source")?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("Video encoder has no sink")?;
        pipeline
            .set_state(gst::State::Playing)
            .context("Could not start the video encoder")?;
        Ok(Self {
            pipeline,
            src,
            sink,
            key: (width, height, format, mode),
            frames: 0,
        })
    }

    /// Encodes one tightly packed 4-byte-per-pixel frame. Blocking; returns the
    /// access unit, whether it is a keyframe, and the WebCodecs codec string.
    pub fn encode(&mut self, pixels: Vec<u8>) -> Result<(Vec<u8>, bool, String)> {
        let (width, height, ..) = self.key;
        ensure!(
            pixels.len() == width as usize * height as usize * 4,
            "Frame does not match the encoder size"
        );
        let mut buffer = gst::Buffer::from_mut_slice(pixels);
        {
            let buffer = buffer.get_mut().context("Frame buffer is shared")?;
            // Timestamps only order frames; the stream itself is not timed.
            buffer.set_pts(gst::ClockTime::from_mseconds(self.frames * 16));
        }
        self.frames += 1;
        self.src
            .push_buffer(buffer)
            .map_err(|error| anyhow::anyhow!("Video encoder refused a frame: {error:?}"))?;
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(2))
            .context("Video encoder produced no frame")?;
        let buffer = sample.buffer().context("Encoded frame is empty")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let data = buffer.map_readable()?.to_vec();
        ensure!(
            data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]),
            "Encoded frame is not Annex B"
        );
        Ok((data, keyframe, codec_string(sample.caps())))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// `hev1.<profile>.<compat>.<tier><level>.<constraints>` for WebCodecs, from
/// h265parse's caps. Main profile; the level follows the frame size.
fn codec_string(caps: Option<&gst::CapsRef>) -> String {
    let level = caps
        .and_then(|caps| caps.structure(0))
        .and_then(|s| s.get::<String>("level").ok())
        .and_then(|level| level.parse::<f64>().ok())
        .map(|level| (level * 30.0).round() as u32)
        .unwrap_or(153);
    format!("hev1.1.6.L{level}.B0")
}

/// What the view's worker controls while the source runs.
#[derive(Debug, Default)]
pub(crate) struct SourceControl {
    pub mode: Mode,
    /// Start the next frame with a new encoder (a keyframe).
    pub restart: bool,
}

/// One encoded frame and what it shows.
pub(crate) struct Encoded {
    pub window: FocusedWindow,
    pub crop: FocusRect,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub codec: String,
    pub width: u16,
    pub height: u16,
    pub capture: Duration,
    pub encode: Duration,
}

/// The window's region on its monitor, in that monitor's logical coordinates.
fn region(window: &FocusedWindow, crop: &FocusRect) -> (i32, i32, i32, i32) {
    let monitor = &window.monitor.logical_bounds;
    (
        crop.x - monitor.x,
        crop.y - monitor.y,
        crop.width as i32,
        crop.height as i32,
    )
}

type Job = (Vec<u8>, bool, String, u32, u32, Duration, Duration);

/// Captures and encodes the selected window until `frames` closes, at most
/// one frame per [`Mode::interval`] and only when the region changed (or a
/// [`KEEPALIVE`] is due). The one-slot channel is the backpressure: nothing is
/// captured while the worker can't send.
pub(crate) async fn source(
    control: Arc<Mutex<SourceControl>>,
    frames: mpsc::Sender<Result<Encoded>>,
) {
    let mut copier: Option<ScreenCopy> = None;
    let mut encoder: Option<Encoder> = None;
    let mut last_sent = Instant::now() - KEEPALIVE;
    let mut last_window: Option<FocusedWindow> = None;
    let mut unlocked_at: Option<Instant> = None;
    loop {
        let started = Instant::now();
        let (mode, restart) = {
            let mut control = control.lock().unwrap_or_else(|p| p.into_inner());
            (control.mode, std::mem::take(&mut control.restart))
        };
        // The lock check is a separate request, so it runs every KEEPALIVE;
        // the window is sampled for every frame.
        let check_lock = unlocked_at.is_none_or(|at| at.elapsed() > KEEPALIVE);
        let window = async {
            if check_lock {
                crate::phone_remote_view::ensure_unlocked().await?;
            }
            crate::phone_remote_view::sample_window().await
        }
        .await;
        let before = match window {
            Ok(window) => {
                if check_lock {
                    unlocked_at = Some(Instant::now());
                }
                window
            }
            Err(error) => {
                unlocked_at = None;
                if frames.send(Err(error)).await.is_err() {
                    return;
                }
                tokio::time::sleep(KEEPALIVE).await;
                continue;
            }
        };
        let Some(crop) = before.visible_bounds.clone() else {
            if frames
                .send(Err(anyhow::anyhow!(
                    "Selected window is outside the visible monitor"
                )))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(KEEPALIVE).await;
            continue;
        };
        // A new window, a restart or a due keepalive copies at once; otherwise
        // wait (up to one interval) for something to change.
        let moved = last_window
            .as_ref()
            .is_none_or(|w| !crate::phone_remote_view::same_target(w, &before));
        let wait_for_damage = !(moved || restart || last_sent.elapsed() >= KEEPALIVE);
        let output = before.monitor.name.clone();
        let rect = region(&before, &crop);
        let wait = mode.interval();
        let mut job_copier = copier.take();
        let mut job_encoder = encoder.take();
        let job = tokio::task::spawn_blocking(move || {
            let result: Result<Option<Job>> = (|| {
                if job_copier.is_none() {
                    job_copier = Some(ScreenCopy::connect().context("Screen capture unavailable")?);
                }
                let capture_started = Instant::now();
                let captured =
                    match job_copier
                        .as_mut()
                        .unwrap()
                        .capture(&output, rect, wait_for_damage, wait)
                    {
                        Ok(captured) => captured,
                        Err(error) => {
                            job_copier = None;
                            return Err(anyhow::anyhow!(error.to_string()));
                        }
                    };
                let capture = capture_started.elapsed();
                let Some(CapturedRegion {
                    width,
                    height,
                    format,
                    data,
                }) = captured
                else {
                    return Ok(None);
                };
                let encode_started = Instant::now();
                let key = (width, height, format, mode);
                if restart || job_encoder.as_ref().is_none_or(|e| e.key != key) {
                    job_encoder = None;
                    job_encoder = Some(Encoder::new(width, height, format, mode)?);
                }
                let (data, keyframe, codec) = job_encoder.as_mut().unwrap().encode(data)?;
                Ok(Some((
                    data,
                    keyframe,
                    codec,
                    width,
                    height,
                    capture,
                    encode_started.elapsed(),
                )))
            })();
            (job_copier, job_encoder, result)
        });
        let Ok((kept_copier, kept_encoder, result)) = job.await else {
            return;
        };
        copier = kept_copier;
        encoder = kept_encoder;
        let (data, keyframe, codec, width, height, capture, encode) = match result {
            Ok(Some(encoded)) => encoded,
            Ok(None) => continue,
            Err(error) => {
                encoder = None;
                if frames.send(Err(error)).await.is_err() {
                    return;
                }
                tokio::time::sleep(KEEPALIVE).await;
                continue;
            }
        };
        // Only send what is still the selected window, where it was.
        let after = crate::phone_remote_view::sample_window().await;
        if !after
            .as_ref()
            .is_ok_and(|a| crate::phone_remote_view::same_target(&before, a))
        {
            last_window = None;
            continue;
        }
        last_window = Some(before.clone());
        last_sent = Instant::now();
        let frame = Encoded {
            window: before,
            crop,
            data,
            keyframe,
            codec,
            width: width as u16,
            height: height as u16,
            capture,
            encode,
        };
        if frames.send(Ok(frame)).await.is_err() {
            return;
        }
        tokio::time::sleep(mode.interval().saturating_sub(started.elapsed())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_trade_sharpness_for_frame_rate() {
        assert_eq!(Mode::default(), Mode::Text);
        assert!(Mode::Media.interval() < Mode::Text.interval());
        assert!(Mode::Media.in_flight() > Mode::Text.in_flight());
        assert!(Mode::Media.interval() <= Duration::from_millis(17));
    }

    /// The real capture → encode loop on the focused window, per mode.
    #[tokio::test]
    #[ignore = "requires Hyprland, screencopy and NVENC"]
    async fn source_streams_the_focused_window_in_both_modes() {
        for mode in [Mode::Text, Mode::Media] {
            let control = Arc::new(Mutex::new(SourceControl {
                mode,
                restart: false,
            }));
            let (tx, mut rx) = mpsc::channel(1);
            let task = tokio::spawn(source(control, tx));
            let started = Instant::now();
            let (mut frames, mut keyframes, mut bytes) = (0, 0, 0);
            let (mut capture, mut encode) = (Duration::ZERO, Duration::ZERO);
            while started.elapsed() < Duration::from_secs(4) {
                let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
                else {
                    break;
                };
                let frame = frame.unwrap();
                frames += 1;
                keyframes += u32::from(frame.keyframe);
                bytes += frame.data.len();
                capture += frame.capture;
                encode += frame.encode;
            }
            task.abort();
            println!(
                "{mode:?}: {frames} frames in 4 s ({:.1} fps), {keyframes} keyframes, {} KB/s, capture {:.1} ms, encode {:.1} ms per frame",
                f64::from(frames) / 4.0,
                bytes / 4 / 1024,
                capture.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
                encode.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            );
            assert!(frames >= 8, "at least the keepalive rate");
            assert_eq!(keyframes, 1);
        }
    }

    #[test]
    #[ignore = "requires GStreamer with NVENC"]
    fn encodes_one_access_unit_per_frame_starting_with_a_keyframe() {
        for mode in [Mode::Text, Mode::Media] {
            let mut encoder = Encoder::new(874, 1428, "BGRx", mode).unwrap();
            let started = Instant::now();
            let (first, key, codec) = encoder.encode(vec![40; 874 * 1428 * 4]).unwrap();
            assert!(key && first.starts_with(&[0, 0, 0, 1]));
            assert!(codec.starts_with("hev1.1.6.L"), "{codec}");
            for i in 0..5 {
                let (delta, key, _) = encoder.encode(vec![40 + i; 874 * 1428 * 4]).unwrap();
                assert!(!key && !delta.is_empty());
            }
            println!(
                "{mode:?}: 6 frames in {:?}; keyframe {} bytes, {codec}",
                started.elapsed(),
                first.len()
            );
        }
    }
}
