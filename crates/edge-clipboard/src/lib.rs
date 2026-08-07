use std::{
    io::Cursor,
    time::{Duration, Instant},
};

use edge_protocol::{ClipboardCancelReason, ClipboardEvent};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbaImage};
use sha2::{Digest, Sha256};

pub const MAX_IMAGE_PIXELS: u64 = 16_777_216;
pub const IMAGE_CHUNK_BYTES: usize = 16 * 1024;
pub const IMAGE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_HIGH_PRIORITY_FRAMES_BEFORE_IMAGE_CHUNK: u8 = 64;

#[derive(Debug, Default)]
pub struct ImageTransferSchedule {
    high_priority_frames_since_chunk: u8,
}

impl ImageTransferSchedule {
    pub fn record_high_priority_frame(&mut self) {
        self.high_priority_frames_since_chunk =
            self.high_priority_frames_since_chunk.saturating_add(1);
    }

    pub fn record_image_chunk(&mut self) {
        self.high_priority_frames_since_chunk = 0;
    }

    pub fn image_chunk_is_due(&self) -> bool {
        self.high_priority_frames_since_chunk >= MAX_HIGH_PRIORITY_FRAMES_BEFORE_IMAGE_CHUNK
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("image dimensions are invalid")]
    InvalidDimensions,
    #[error("image has {pixels} pixels, exceeding the limit of {MAX_IMAGE_PIXELS}")]
    TooManyPixels { pixels: u64 },
    #[error("RGBA buffer length does not match {width}x{height}")]
    InvalidRgbaLength { width: u32, height: u32 },
    #[error("encoded image is {actual} bytes, exceeding the limit of {max}")]
    EncodedTooLarge { actual: usize, max: usize },
    #[error("unsupported clipboard image MIME type: {0}")]
    UnsupportedMime(String),
    #[error("image decode or encode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("image transfer metadata is invalid")]
    InvalidTransfer,
    #[error("image transfer offset mismatch: expected {expected}, got {actual}")]
    OffsetMismatch { expected: u32, actual: u32 },
    #[error("image transfer content hash did not match")]
    HashMismatch,
    #[error("image transfer expired")]
    TransferExpired,
}

pub type Result<T> = std::result::Result<T, ClipboardError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardItem {
    Text(String),
    Image(CanonicalImage),
}

impl ClipboardItem {
    pub fn id(&self) -> ClipboardContentId {
        match self {
            Self::Text(text) => ClipboardContentId::Text(hash_bytes(text.as_bytes())),
            Self::Image(image) => ClipboardContentId::Image(image.content_sha256),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardContentId {
    Text([u8; 32]),
    Image([u8; 32]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub png: Vec<u8>,
    pub content_sha256: [u8; 32],
}

impl CanonicalImage {
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>, max_bytes: usize) -> Result<Self> {
        validate_dimensions(width, height, rgba.len())?;
        let image = RgbaImage::from_raw(width, height, rgba)
            .ok_or(ClipboardError::InvalidRgbaLength { width, height })?;
        let rgba = image.as_raw().clone();
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image).write_to(&mut png, ImageFormat::Png)?;
        let png = png.into_inner();
        if png.len() > max_bytes {
            return Err(ClipboardError::EncodedTooLarge {
                actual: png.len(),
                max: max_bytes,
            });
        }
        Ok(Self {
            width,
            height,
            content_sha256: hash_image(width, height, &rgba),
            rgba,
            png,
        })
    }

    pub fn from_encoded(bytes: &[u8], mime: &str, max_bytes: usize) -> Result<Self> {
        if bytes.len() > max_bytes {
            return Err(ClipboardError::EncodedTooLarge {
                actual: bytes.len(),
                max: max_bytes,
            });
        }
        let format = match mime {
            "image/png" => ImageFormat::Png,
            "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
            "image/bmp" | "image/x-bmp" => ImageFormat::Bmp,
            other => return Err(ClipboardError::UnsupportedMime(other.to_string())),
        };
        let dimensions_reader = ImageReader::with_format(Cursor::new(bytes), format);
        let (width, height) = dimensions_reader.into_dimensions()?;
        let pixels = u64::from(width) * u64::from(height);
        if width == 0 || height == 0 {
            return Err(ClipboardError::InvalidDimensions);
        }
        if pixels > MAX_IMAGE_PIXELS {
            return Err(ClipboardError::TooManyPixels { pixels });
        }
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some(MAX_IMAGE_PIXELS * 4);
        reader.limits(limits);
        let decoded = reader.decode()?;
        Self::from_rgba(width, height, decoded.into_rgba8().into_raw(), max_bytes)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClipboardChangeTracker {
    sequence: u64,
    last_observed: Option<ClipboardContentId>,
}

impl ClipboardChangeTracker {
    pub fn new(last_observed: Option<ClipboardContentId>) -> Self {
        Self {
            sequence: 0,
            last_observed,
        }
    }

    pub fn is_observed(&self, current: &Option<ClipboardContentId>) -> bool {
        &self.last_observed == current
    }

    pub fn mark_observed(&mut self, current: Option<ClipboardContentId>) {
        self.last_observed = current;
    }

    pub fn offer_if_changed(&mut self, current: Option<ClipboardContentId>) -> Option<u64> {
        if self.is_observed(&current) {
            return None;
        }
        self.offer_current(current)
    }

    pub fn offer_current(&mut self, current: Option<ClipboardContentId>) -> Option<u64> {
        self.last_observed = current;
        self.last_observed?;
        self.sequence = self.sequence.saturating_add(1);
        Some(self.sequence)
    }
}

#[derive(Debug, Clone)]
pub struct OutgoingImageTransfer {
    transfer_id: u64,
    sequence: u64,
    image: CanonicalImage,
    offset: usize,
    stage: OutgoingStage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutgoingStage {
    Start,
    Chunks,
    End,
    Done,
}

impl OutgoingImageTransfer {
    pub fn new(transfer_id: u64, sequence: u64, image: CanonicalImage) -> Self {
        Self {
            transfer_id,
            sequence,
            image,
            offset: 0,
            stage: OutgoingStage::Start,
        }
    }

    pub fn transfer_id(&self) -> u64 {
        self.transfer_id
    }

    pub fn is_done(&self) -> bool {
        self.stage == OutgoingStage::Done
    }

    pub fn cancel_event(&self, reason: ClipboardCancelReason) -> ClipboardEvent {
        ClipboardEvent::ImageCancel {
            transfer_id: self.transfer_id,
            reason,
        }
    }

    pub fn next_event(&mut self) -> Option<ClipboardEvent> {
        match self.stage {
            OutgoingStage::Start => {
                self.stage = OutgoingStage::Chunks;
                Some(ClipboardEvent::ImageStart {
                    transfer_id: self.transfer_id,
                    sequence: self.sequence,
                    width: self.image.width,
                    height: self.image.height,
                    total_bytes: self.image.png.len() as u32,
                    content_sha256: self.image.content_sha256,
                })
            }
            OutgoingStage::Chunks => {
                if self.offset >= self.image.png.len() {
                    self.stage = OutgoingStage::End;
                    return self.next_event();
                }
                let end = (self.offset + IMAGE_CHUNK_BYTES).min(self.image.png.len());
                let event = ClipboardEvent::ImageChunk {
                    transfer_id: self.transfer_id,
                    offset: self.offset as u32,
                    bytes: self.image.png[self.offset..end].to_vec(),
                };
                self.offset = end;
                Some(event)
            }
            OutgoingStage::End => {
                self.stage = OutgoingStage::Done;
                Some(ClipboardEvent::ImageEnd {
                    transfer_id: self.transfer_id,
                })
            }
            OutgoingStage::Done => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct IncomingImageTransfer {
    active: Option<IncomingState>,
}

#[derive(Debug)]
struct IncomingState {
    transfer_id: u64,
    width: u32,
    height: u32,
    total_bytes: usize,
    content_sha256: [u8; 32],
    bytes: Vec<u8>,
    last_progress: Instant,
}

impl IncomingImageTransfer {
    pub fn handle(
        &mut self,
        event: ClipboardEvent,
        max_bytes: usize,
    ) -> Result<Option<CanonicalImage>> {
        match event {
            ClipboardEvent::ImageStart {
                transfer_id,
                width,
                height,
                total_bytes,
                content_sha256,
                ..
            } => {
                let total_bytes = total_bytes as usize;
                let rgba_len = u64::from(width)
                    .checked_mul(u64::from(height))
                    .and_then(|pixels| pixels.checked_mul(4))
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .ok_or(ClipboardError::InvalidDimensions)?;
                validate_dimensions(width, height, rgba_len)?;
                if total_bytes == 0 || total_bytes > max_bytes {
                    return Err(ClipboardError::EncodedTooLarge {
                        actual: total_bytes,
                        max: max_bytes,
                    });
                }
                self.active = Some(IncomingState {
                    transfer_id,
                    width,
                    height,
                    total_bytes,
                    content_sha256,
                    bytes: Vec::with_capacity(total_bytes),
                    last_progress: Instant::now(),
                });
                Ok(None)
            }
            ClipboardEvent::ImageChunk {
                transfer_id,
                offset,
                bytes,
            } => {
                let state = self
                    .active
                    .as_mut()
                    .ok_or(ClipboardError::InvalidTransfer)?;
                if state.transfer_id != transfer_id {
                    return Err(ClipboardError::InvalidTransfer);
                }
                let expected = state.bytes.len() as u32;
                if offset != expected {
                    return Err(ClipboardError::OffsetMismatch {
                        expected,
                        actual: offset,
                    });
                }
                if bytes.is_empty()
                    || bytes.len() > IMAGE_CHUNK_BYTES
                    || state.bytes.len().saturating_add(bytes.len()) > state.total_bytes
                {
                    return Err(ClipboardError::InvalidTransfer);
                }
                state.bytes.extend_from_slice(&bytes);
                state.last_progress = Instant::now();
                Ok(None)
            }
            ClipboardEvent::ImageEnd { transfer_id } => {
                let state = self.active.take().ok_or(ClipboardError::InvalidTransfer)?;
                if state.transfer_id != transfer_id || state.bytes.len() != state.total_bytes {
                    return Err(ClipboardError::InvalidTransfer);
                }
                let image = CanonicalImage::from_encoded(&state.bytes, "image/png", max_bytes)?;
                if image.width != state.width
                    || image.height != state.height
                    || image.content_sha256 != state.content_sha256
                {
                    return Err(ClipboardError::HashMismatch);
                }
                Ok(Some(image))
            }
            ClipboardEvent::ImageCancel { transfer_id, .. } => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.transfer_id == transfer_id)
                {
                    self.active = None;
                }
                Ok(None)
            }
            _ => Err(ClipboardError::InvalidTransfer),
        }
    }

    pub fn expire(&mut self) -> Result<()> {
        if self.expire_transfer_id().is_some() {
            return Err(ClipboardError::TransferExpired);
        }
        Ok(())
    }

    pub fn expire_transfer_id(&mut self) -> Option<u64> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.last_progress.elapsed() > IMAGE_TRANSFER_TIMEOUT)
        {
            return self.active.take().map(|active| active.transfer_id);
        }
        None
    }

    pub fn clear(&mut self) {
        self.active = None;
    }
}

fn validate_dimensions(width: u32, height: u32, rgba_len: usize) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(ClipboardError::InvalidDimensions);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(ClipboardError::TooManyPixels { pixels });
    }
    let expected = pixels
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(ClipboardError::InvalidDimensions)?;
    if rgba_len != expected {
        return Err(ClipboardError::InvalidRgbaLength { width, height });
    }
    Ok(())
}

fn hash_image(width: u32, height: u32, rgba: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(width.to_be_bytes());
    hasher.update(height.to_be_bytes());
    hasher.update(rgba);
    hasher.finalize().into()
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image() -> CanonicalImage {
        CanonicalImage::from_rgba(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 255, 0,
            ],
            1024,
        )
        .unwrap()
    }

    #[test]
    fn canonical_png_preserves_rgba_and_identity() {
        let image = sample_image();
        let decoded = CanonicalImage::from_encoded(&image.png, "image/png", 1024).unwrap();
        assert_eq!(decoded.width, image.width);
        assert_eq!(decoded.height, image.height);
        assert_eq!(decoded.rgba, image.rgba);
        assert_eq!(decoded.content_sha256, image.content_sha256);
    }

    #[test]
    fn transfer_round_trip_and_offset_validation() {
        let image = sample_image();
        let mut outgoing = OutgoingImageTransfer::new(7, 3, image.clone());
        let mut incoming = IncomingImageTransfer::default();
        let mut completed = None;
        while let Some(event) = outgoing.next_event() {
            completed = incoming.handle(event, 1024).unwrap().or(completed);
        }
        assert_eq!(completed.unwrap(), image);

        let mut incoming = IncomingImageTransfer::default();
        incoming
            .handle(
                ClipboardEvent::ImageStart {
                    transfer_id: 1,
                    sequence: 1,
                    width: 1,
                    height: 1,
                    total_bytes: 4,
                    content_sha256: [0; 32],
                },
                1024,
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 1,
                    offset: 2,
                    bytes: vec![1],
                },
                1024
            ),
            Err(ClipboardError::OffsetMismatch { .. })
        ));
    }

    #[test]
    fn tracker_suppresses_remote_echo() {
        let id = ClipboardItem::Image(sample_image()).id();
        let mut tracker = ClipboardChangeTracker::new(None);
        assert_eq!(tracker.offer_if_changed(Some(id)), Some(1));
        tracker.mark_observed(Some(id));
        assert_eq!(tracker.offer_if_changed(Some(id)), None);
    }

    #[test]
    fn jpeg_and_bmp_normalize_to_png() {
        for (format, mime) in [
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Bmp, "image/bmp"),
        ] {
            let source =
                DynamicImage::ImageRgba8(RgbaImage::from_raw(1, 1, vec![20, 40, 60, 255]).unwrap());
            let mut encoded = Cursor::new(Vec::new());
            source.write_to(&mut encoded, format).unwrap();
            let canonical =
                CanonicalImage::from_encoded(&encoded.into_inner(), mime, 1024).unwrap();
            assert_eq!((canonical.width, canonical.height), (1, 1));
            assert!(canonical.png.starts_with(b"\x89PNG"));
        }
    }

    #[test]
    fn rejects_pixel_and_encoded_size_limits() {
        assert!(matches!(
            CanonicalImage::from_rgba(5000, 5000, Vec::new(), usize::MAX),
            Err(ClipboardError::TooManyPixels { .. })
        ));
        assert!(matches!(
            CanonicalImage::from_encoded(&[0; 5], "image/png", 4),
            Err(ClipboardError::EncodedTooLarge { .. })
        ));
    }

    #[test]
    fn cancellation_and_timeout_clear_incoming_transfer() {
        let image = sample_image();
        let mut incoming = IncomingImageTransfer::default();
        incoming
            .handle(
                ClipboardEvent::ImageStart {
                    transfer_id: 1,
                    sequence: 1,
                    width: image.width,
                    height: image.height,
                    total_bytes: image.png.len() as u32,
                    content_sha256: image.content_sha256,
                },
                1024,
            )
            .unwrap();
        incoming
            .handle(
                ClipboardEvent::ImageCancel {
                    transfer_id: 1,
                    reason: ClipboardCancelReason::Replaced,
                },
                1024,
            )
            .unwrap();
        assert!(incoming.active.is_none());

        incoming
            .handle(
                ClipboardEvent::ImageStart {
                    transfer_id: 2,
                    sequence: 2,
                    width: image.width,
                    height: image.height,
                    total_bytes: image.png.len() as u32,
                    content_sha256: image.content_sha256,
                },
                1024,
            )
            .unwrap();
        incoming.active.as_mut().unwrap().last_progress =
            Instant::now() - IMAGE_TRANSFER_TIMEOUT - Duration::from_millis(1);
        assert!(matches!(
            incoming.expire(),
            Err(ClipboardError::TransferExpired)
        ));
        assert!(incoming.active.is_none());
    }

    #[test]
    fn image_transfer_schedule_prevents_permanent_starvation() {
        let mut schedule = ImageTransferSchedule::default();
        for _ in 0..MAX_HIGH_PRIORITY_FRAMES_BEFORE_IMAGE_CHUNK - 1 {
            schedule.record_high_priority_frame();
            assert!(!schedule.image_chunk_is_due());
        }
        schedule.record_high_priority_frame();
        assert!(schedule.image_chunk_is_due());

        schedule.record_image_chunk();
        assert!(!schedule.image_chunk_is_due());
    }
}
