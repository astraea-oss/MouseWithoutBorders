//! Video remote view (experimental, `phone-window-video-v1`): raw window
//! captures encoded to HEVC on NVENC through GStreamer. One access unit
//! (Annex B, parameter sets repeated on every keyframe) comes out per frame in,
//! so frames keep the JPEG view's frame-scoped metadata and input rules.
use anyhow::{Context, Result, bail, ensure};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::OnceLock;

/// Whether this computer can encode the stream: GStreamer with NVENC HEVC.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        gst::init().is_ok()
            && [
                "nvh265enc",
                "h265parse",
                "videoconvert",
                "appsrc",
                "appsink",
            ]
            .iter()
            .all(|name| gst::ElementFactory::find(name).is_some())
    })
}

/// One NVENC session for a fixed frame size. A new size, or a broken decoding
/// chain on the phone, gets a new encoder, whose first frame is a keyframe.
pub(crate) struct Encoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    pub width: u32,
    pub height: u32,
    frames: u64,
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(available(), "This computer can't encode HEVC video");
        ensure!(
            width >= 16 && height >= 16 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "Invalid video size {width}x{height}"
        );
        // Text mode: constant quality, low latency, no reordering, one long GOP.
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false \
               caps=video/x-raw,format=RGB,width={width},height={height},framerate=0/1 \
             ! videoconvert ! video/x-raw,format=NV12 \
             ! nvh265enc preset=p4 tune=low-latency zerolatency=true bframes=0 gop-size=-1 \
               rc-mode=vbr const-quality=20 repeat-sequence-header=true \
             ! h265parse config-interval=-1 \
             ! video/x-h265,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=2"
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
            width,
            height,
            frames: 0,
        })
    }

    /// Encodes one RGB frame. Blocking; returns the access unit, whether it is
    /// a keyframe, and the WebCodecs codec string.
    pub fn encode(&mut self, rgb: Vec<u8>) -> Result<(Vec<u8>, bool, String)> {
        ensure!(
            rgb.len() == self.width as usize * self.height as usize * 3,
            "Frame does not match the encoder size"
        );
        let mut buffer = gst::Buffer::from_mut_slice(padded_rows(rgb, self.width, self.height));
        {
            let buffer = buffer.get_mut().context("Frame buffer is shared")?;
            // Timestamps only order frames; the stream itself is not timed.
            buffer.set_pts(gst::ClockTime::from_mseconds(self.frames * 100));
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

/// GStreamer's default `video/x-raw` layout pads every row to a multiple of 4
/// bytes. Tightly packed RGB of a width that isn't a multiple of 4 would be
/// the wrong size and encode as garbage, so pad those rows.
fn padded_rows(rgb: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 3;
    let stride = row.next_multiple_of(4);
    if stride == row {
        return rgb;
    }
    let mut padded = vec![0; stride * height as usize];
    for (from, to) in rgb.chunks_exact(row).zip(padded.chunks_exact_mut(stride)) {
        to[..row].copy_from_slice(from);
    }
    padded
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

/// A binary PPM (`P6`, 8-bit) from grim, cropped to even dimensions for
/// 4:2:0 encoding. Returns width, height and RGB bytes.
pub(crate) fn rgb_from_ppm(ppm: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while at < ppm.len() && ppm[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while at < ppm.len() && !ppm[at].is_ascii_whitespace() {
            at += 1;
        }
        ensure!(at > start && at < ppm.len(), "Truncated capture header");
        fields.push(std::str::from_utf8(&ppm[start..at])?);
    }
    at += 1; // The single whitespace byte before the pixels.
    ensure!(
        fields[0] == "P6" && fields[3] == "255",
        "Unexpected capture format"
    );
    let width: u32 = fields[1].parse()?;
    let height: u32 = fields[2].parse()?;
    let row = width as usize * 3;
    let pixels = &ppm[at..];
    if pixels.len() != row * height as usize {
        bail!(
            "Capture is {} bytes, expected {}",
            pixels.len(),
            row * height as usize
        );
    }
    let (even_width, even_height) = (width & !1, height & !1);
    ensure!(
        even_width >= 16 && even_height >= 16,
        "Window is too small to stream"
    );
    if even_width == width && even_height == height {
        return Ok((width, height, pixels.to_vec()));
    }
    let even_row = even_width as usize * 3;
    let mut rgb = Vec::with_capacity(even_row * even_height as usize);
    for line in pixels.chunks_exact(row).take(even_height as usize) {
        rgb.extend_from_slice(&line[..even_row]);
    }
    Ok((even_width, even_height, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(width: u32, height: u32) -> Vec<u8> {
        let mut data = format!("P6\n{width} {height}\n255\n").into_bytes();
        for y in 0..height {
            for x in 0..width {
                data.extend([x as u8, y as u8, 7]);
            }
        }
        data
    }

    #[test]
    fn rgb_rows_are_padded_to_gstreamer_strides() {
        let rgb: Vec<u8> = (0..18 * 2 * 3).map(|i| i as u8).collect();
        let padded = padded_rows(rgb.clone(), 18, 2);
        // 18 × 3 = 54 bytes per row, padded to 56.
        assert_eq!(padded.len(), 112);
        assert_eq!(&padded[..54], &rgb[..54]);
        assert_eq!(&padded[56..110], &rgb[54..]);
        assert_eq!(padded_rows(vec![1; 16 * 2 * 3], 16, 2), vec![1; 96]);
    }

    #[test]
    fn ppm_is_cropped_to_even_dimensions() {
        let (w, h, rgb) = rgb_from_ppm(&ppm(17, 19)).unwrap();
        assert_eq!((w, h, rgb.len()), (16, 18, 16 * 18 * 3));
        // Row 1 starts with pixel (0, 1): the dropped column doesn't shift rows.
        assert_eq!(&rgb[16 * 3..16 * 3 + 3], &[0, 1, 7]);
        let (w, h, rgb) = rgb_from_ppm(&ppm(16, 16)).unwrap();
        assert_eq!((w, h, rgb.len()), (16, 16, 768));
        assert!(rgb_from_ppm(&ppm(16, 16)[..20]).is_err());
        assert!(rgb_from_ppm(b"P5\n16 16\n255\n").is_err());
    }

    /// Writes an encoded real capture to $HYPR_VIDEO_DUMP for inspection.
    #[test]
    #[ignore = "requires Hyprland, grim and NVENC"]
    fn dump_encoded_capture() {
        let Ok(path) = std::env::var("HYPR_VIDEO_DUMP") else {
            return;
        };
        let mut grim = std::process::Command::new("grim");
        grim.args(["-t", "ppm"]);
        if let Ok(geometry) = std::env::var("HYPR_VIDEO_GEOMETRY") {
            grim.args(["-g", &geometry]);
        }
        let output = grim.arg("-").output().unwrap();
        let (w, h, rgb) = rgb_from_ppm(&output.stdout).unwrap();
        let nonzero = rgb.iter().filter(|b| **b != 0).count();
        println!(
            "capture {w}x{h}, {} of {} bytes nonzero",
            nonzero,
            rgb.len()
        );
        let mut encoder = Encoder::new(w, h).unwrap();
        let mut stream = Vec::new();
        for _ in 0..3 {
            let (au, key, codec) = encoder.encode(rgb.clone()).unwrap();
            println!("access unit {} bytes, keyframe {key}, {codec}", au.len());
            stream.extend(au);
        }
        std::fs::write(path, stream).unwrap();
    }

    #[test]
    #[ignore = "requires GStreamer with NVENC"]
    fn encodes_one_access_unit_per_frame_starting_with_a_keyframe() {
        let mut encoder = Encoder::new(874, 1428).unwrap();
        let started = std::time::Instant::now();
        let (first, key, codec) = encoder.encode(vec![40; 874 * 1428 * 3]).unwrap();
        assert!(key && first.starts_with(&[0, 0, 0, 1]));
        assert!(codec.starts_with("hev1.1.6.L"), "{codec}");
        for i in 0..5 {
            let (delta, key, _) = encoder.encode(vec![40 + i; 874 * 1428 * 3]).unwrap();
            assert!(!key && !delta.is_empty());
        }
        println!(
            "6 frames in {:?}; keyframe {} bytes, codec {codec}",
            started.elapsed(),
            first.len()
        );
    }
}
