use std::{
    io::Cursor,
    time::{Duration, Instant},
};

use edge_protocol::{
    CLIPBOARD_IMAGE_EXTENSION, CLIPBOARD_TEXT_CHUNKS_EXTENSION, ClipboardCancelReason,
    ClipboardEvent, Frame,
};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbaImage};
use sha2::{Digest, Sha256};

pub const MAX_IMAGE_PIXELS: u64 = 16_777_216;
/// Chunk size for image and text clipboard transfers.
pub const TRANSFER_CHUNK_BYTES: usize = 16 * 1024;
pub const IMAGE_CHUNK_BYTES: usize = TRANSFER_CHUNK_BYTES;
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

    /// Records a frame this peer sent. Chunks reset the budget; everything else
    /// spends it.
    pub fn record_sent_frame(&mut self, frame: &Frame) {
        if matches!(
            frame,
            Frame::Clipboard(ClipboardEvent::ImageChunk { .. } | ClipboardEvent::TextChunk { .. })
        ) {
            self.record_image_chunk();
        } else {
            self.record_high_priority_frame();
        }
    }

    /// Records a frame this peer received.
    ///
    /// A receiver under remote control sends almost nothing of its own, so a
    /// budget spent only by outbound frames advances at the heartbeat rate and
    /// leaves an outgoing image transfer starved behind inbound input. Inbound
    /// work has to count too. Clipboard frames are excluded: they belong to the
    /// transfer itself and must not spend its own starvation budget.
    pub fn record_received_frame(&mut self, frame: &Frame) {
        if !matches!(frame, Frame::Clipboard(_)) {
            self.record_high_priority_frame();
        }
    }
}

/// Clipboard transfer features negotiated through `Hello` extensions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardPeerFeatures {
    pub images: bool,
    pub text_chunks: bool,
}

impl ClipboardPeerFeatures {
    pub fn from_extensions(extensions: &[String]) -> Self {
        let has = |name: &str| extensions.iter().any(|extension| extension == name);
        Self {
            images: has(CLIPBOARD_IMAGE_EXTENSION),
            text_chunks: has(CLIPBOARD_TEXT_CHUNKS_EXTENSION),
        }
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
    #[error("clipboard text transfer is not valid UTF-8")]
    InvalidText,
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
enum OutgoingPayload {
    Image(CanonicalImage),
    Text {
        bytes: Vec<u8>,
        content_sha256: [u8; 32],
    },
}

impl OutgoingPayload {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Image(image) => &image.png,
            Self::Text { bytes, .. } => bytes,
        }
    }
}

/// A clipboard item sent as a start frame, bounded chunks, and an end frame.
#[derive(Debug, Clone)]
pub struct OutgoingClipboardTransfer {
    transfer_id: u64,
    sequence: u64,
    payload: OutgoingPayload,
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

impl OutgoingClipboardTransfer {
    pub fn image(transfer_id: u64, sequence: u64, image: CanonicalImage) -> Self {
        Self::with_payload(transfer_id, sequence, OutgoingPayload::Image(image))
    }

    pub fn text(transfer_id: u64, sequence: u64, text: String) -> Self {
        let bytes = text.into_bytes();
        let content_sha256 = hash_bytes(&bytes);
        Self::with_payload(
            transfer_id,
            sequence,
            OutgoingPayload::Text {
                bytes,
                content_sha256,
            },
        )
    }

    fn with_payload(transfer_id: u64, sequence: u64, payload: OutgoingPayload) -> Self {
        Self {
            transfer_id,
            sequence,
            payload,
            offset: 0,
            stage: OutgoingStage::Start,
        }
    }

    pub fn transfer_id(&self) -> u64 {
        self.transfer_id
    }

    pub fn is_text(&self) -> bool {
        matches!(self.payload, OutgoingPayload::Text { .. })
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
                let total_bytes = self.payload.bytes().len() as u32;
                Some(match &self.payload {
                    OutgoingPayload::Image(image) => ClipboardEvent::ImageStart {
                        transfer_id: self.transfer_id,
                        sequence: self.sequence,
                        width: image.width,
                        height: image.height,
                        total_bytes,
                        content_sha256: image.content_sha256,
                    },
                    OutgoingPayload::Text { content_sha256, .. } => ClipboardEvent::TextStart {
                        transfer_id: self.transfer_id,
                        sequence: self.sequence,
                        total_bytes,
                        content_sha256: *content_sha256,
                    },
                })
            }
            OutgoingStage::Chunks => {
                let payload = self.payload.bytes();
                if self.offset >= payload.len() {
                    self.stage = OutgoingStage::End;
                    return self.next_event();
                }
                let end = (self.offset + TRANSFER_CHUNK_BYTES).min(payload.len());
                let bytes = payload[self.offset..end].to_vec();
                let offset = self.offset as u32;
                self.offset = end;
                Some(match self.payload {
                    OutgoingPayload::Image(_) => ClipboardEvent::ImageChunk {
                        transfer_id: self.transfer_id,
                        offset,
                        bytes,
                    },
                    OutgoingPayload::Text { .. } => ClipboardEvent::TextChunk {
                        transfer_id: self.transfer_id,
                        offset,
                        bytes,
                    },
                })
            }
            OutgoingStage::End => {
                self.stage = OutgoingStage::Done;
                Some(match self.payload {
                    OutgoingPayload::Image(_) => ClipboardEvent::ImageEnd {
                        transfer_id: self.transfer_id,
                    },
                    OutgoingPayload::Text { .. } => ClipboardEvent::TextEnd {
                        transfer_id: self.transfer_id,
                    },
                })
            }
            OutgoingStage::Done => None,
        }
    }
}

/// Size limits applied to incoming chunked clipboard transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferLimits {
    pub max_image_bytes: usize,
    pub max_text_bytes: usize,
}

#[derive(Debug, Default)]
pub struct IncomingClipboardTransfer {
    active: Option<IncomingState>,
}

#[derive(Debug)]
enum IncomingKind {
    Image { width: u32, height: u32 },
    Text,
}

#[derive(Debug)]
struct IncomingState {
    transfer_id: u64,
    kind: IncomingKind,
    total_bytes: usize,
    content_sha256: [u8; 32],
    bytes: Vec<u8>,
    last_progress: Instant,
}

impl IncomingClipboardTransfer {
    pub fn handle(
        &mut self,
        event: ClipboardEvent,
        limits: TransferLimits,
    ) -> Result<Option<ClipboardItem>> {
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
                self.start(
                    transfer_id,
                    IncomingKind::Image { width, height },
                    total_bytes,
                    limits.max_image_bytes,
                    content_sha256,
                )
            }
            ClipboardEvent::TextStart {
                transfer_id,
                total_bytes,
                content_sha256,
                ..
            } => self.start(
                transfer_id,
                IncomingKind::Text,
                total_bytes as usize,
                limits.max_text_bytes,
                content_sha256,
            ),
            ClipboardEvent::ImageChunk {
                transfer_id,
                offset,
                bytes,
            } => self.chunk(transfer_id, false, offset, &bytes),
            ClipboardEvent::TextChunk {
                transfer_id,
                offset,
                bytes,
            } => self.chunk(transfer_id, true, offset, &bytes),
            ClipboardEvent::ImageEnd { transfer_id } => self.finish(transfer_id, false, limits),
            ClipboardEvent::TextEnd { transfer_id } => self.finish(transfer_id, true, limits),
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

    fn start(
        &mut self,
        transfer_id: u64,
        kind: IncomingKind,
        total_bytes: usize,
        max_bytes: usize,
        content_sha256: [u8; 32],
    ) -> Result<Option<ClipboardItem>> {
        if total_bytes == 0 || total_bytes > max_bytes {
            return Err(ClipboardError::EncodedTooLarge {
                actual: total_bytes,
                max: max_bytes,
            });
        }
        self.active = Some(IncomingState {
            transfer_id,
            kind,
            total_bytes,
            content_sha256,
            bytes: Vec::with_capacity(total_bytes),
            last_progress: Instant::now(),
        });
        Ok(None)
    }

    fn chunk(
        &mut self,
        transfer_id: u64,
        text: bool,
        offset: u32,
        bytes: &[u8],
    ) -> Result<Option<ClipboardItem>> {
        let state = self
            .active
            .as_mut()
            .ok_or(ClipboardError::InvalidTransfer)?;
        if state.transfer_id != transfer_id || matches!(state.kind, IncomingKind::Text) != text {
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
            || bytes.len() > TRANSFER_CHUNK_BYTES
            || state.bytes.len().saturating_add(bytes.len()) > state.total_bytes
        {
            return Err(ClipboardError::InvalidTransfer);
        }
        state.bytes.extend_from_slice(bytes);
        state.last_progress = Instant::now();
        Ok(None)
    }

    fn finish(
        &mut self,
        transfer_id: u64,
        text: bool,
        limits: TransferLimits,
    ) -> Result<Option<ClipboardItem>> {
        let state = self.active.take().ok_or(ClipboardError::InvalidTransfer)?;
        if state.transfer_id != transfer_id
            || matches!(state.kind, IncomingKind::Text) != text
            || state.bytes.len() != state.total_bytes
        {
            return Err(ClipboardError::InvalidTransfer);
        }
        match state.kind {
            IncomingKind::Image { width, height } => {
                let image = CanonicalImage::from_encoded(
                    &state.bytes,
                    "image/png",
                    limits.max_image_bytes,
                )?;
                if image.width != width
                    || image.height != height
                    || image.content_sha256 != state.content_sha256
                {
                    return Err(ClipboardError::HashMismatch);
                }
                Ok(Some(ClipboardItem::Image(image)))
            }
            IncomingKind::Text => {
                if hash_bytes(&state.bytes) != state.content_sha256 {
                    return Err(ClipboardError::HashMismatch);
                }
                let text =
                    String::from_utf8(state.bytes).map_err(|_| ClipboardError::InvalidText)?;
                Ok(Some(ClipboardItem::Text(text)))
            }
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

    fn limits(max_bytes: usize) -> TransferLimits {
        TransferLimits {
            max_image_bytes: max_bytes,
            max_text_bytes: max_bytes,
        }
    }

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
        let mut outgoing = OutgoingClipboardTransfer::image(7, 3, image.clone());
        let mut incoming = IncomingClipboardTransfer::default();
        let mut completed = None;
        while let Some(event) = outgoing.next_event() {
            completed = incoming.handle(event, limits(1024)).unwrap().or(completed);
        }
        assert_eq!(completed.unwrap(), ClipboardItem::Image(image));

        let mut incoming = IncomingClipboardTransfer::default();
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
                limits(1024),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 1,
                    offset: 2,
                    bytes: vec![1],
                },
                limits(1024)
            ),
            Err(ClipboardError::OffsetMismatch { .. })
        ));
    }

    #[test]
    fn text_transfer_round_trips_across_chunks() {
        let text = "héllo wörld ".repeat(8_000);
        assert!(text.len() > TRANSFER_CHUNK_BYTES * 4);
        let mut outgoing = OutgoingClipboardTransfer::text(5, 2, text.clone());
        assert!(outgoing.is_text());
        let mut incoming = IncomingClipboardTransfer::default();
        let mut completed = None;
        let mut chunks = 0;
        while let Some(event) = outgoing.next_event() {
            if let ClipboardEvent::TextChunk { bytes, .. } = &event {
                assert!(bytes.len() <= TRANSFER_CHUNK_BYTES);
                chunks += 1;
            }
            assert!(event.is_text_transfer());
            completed = incoming
                .handle(event, limits(text.len()))
                .unwrap()
                .or(completed);
        }
        assert!(outgoing.is_done());
        assert!(chunks > 4);
        assert_eq!(completed, Some(ClipboardItem::Text(text)));
    }

    #[test]
    fn text_transfer_enforces_size_hash_and_kind() {
        let mut incoming = IncomingClipboardTransfer::default();
        assert!(matches!(
            incoming.handle(
                ClipboardEvent::TextStart {
                    transfer_id: 1,
                    sequence: 1,
                    total_bytes: 2048,
                    content_sha256: [0; 32],
                },
                limits(1024),
            ),
            Err(ClipboardError::EncodedTooLarge { .. })
        ));

        incoming
            .handle(
                ClipboardEvent::TextStart {
                    transfer_id: 2,
                    sequence: 1,
                    total_bytes: 3,
                    content_sha256: [0; 32],
                },
                limits(1024),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 2,
                    offset: 0,
                    bytes: b"abc".to_vec(),
                },
                limits(1024),
            ),
            Err(ClipboardError::InvalidTransfer)
        ));
        incoming
            .handle(
                ClipboardEvent::TextChunk {
                    transfer_id: 2,
                    offset: 0,
                    bytes: b"abc".to_vec(),
                },
                limits(1024),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(ClipboardEvent::TextEnd { transfer_id: 2 }, limits(1024)),
            Err(ClipboardError::HashMismatch)
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
        let mut incoming = IncomingClipboardTransfer::default();
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
                limits(1024),
            )
            .unwrap();
        incoming
            .handle(
                ClipboardEvent::ImageCancel {
                    transfer_id: 1,
                    reason: ClipboardCancelReason::Replaced,
                },
                limits(1024),
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
                limits(1024),
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

    fn motion_frame() -> Frame {
        Frame::input(
            edge_protocol::INITIAL_ROLE_EPOCH,
            edge_protocol::InputEvent::PointerMotion { dx: 1.0, dy: 0.0 },
        )
    }

    #[test]
    fn sustained_inbound_input_still_schedules_image_chunks() {
        // Regression: the receiver's biased select polls inbound controller
        // frames ahead of the image-chunk timer, and its own outbound traffic
        // during remote control is essentially just heartbeats. Spending the
        // starvation budget on sent frames alone meant a chunk came due only
        // every ~16s, past the peer's 10s transfer timeout.
        let mut schedule = ImageTransferSchedule::default();
        for _ in 0..MAX_HIGH_PRIORITY_FRAMES_BEFORE_IMAGE_CHUNK {
            assert!(!schedule.image_chunk_is_due());
            schedule.record_received_frame(&motion_frame());
        }
        assert!(schedule.image_chunk_is_due());

        schedule.record_image_chunk();
        assert!(!schedule.image_chunk_is_due());
    }

    #[test]
    fn transfer_frames_do_not_spend_their_own_starvation_budget() {
        let mut schedule = ImageTransferSchedule::default();
        for _ in 0..MAX_HIGH_PRIORITY_FRAMES_BEFORE_IMAGE_CHUNK * 2 {
            schedule.record_received_frame(&Frame::Clipboard(ClipboardEvent::ContentRequest));
        }
        assert!(!schedule.image_chunk_is_due());

        let chunk = Frame::Clipboard(ClipboardEvent::ImageChunk {
            transfer_id: 1,
            offset: 0,
            bytes: vec![0; 8],
        });
        schedule.record_sent_frame(&motion_frame());
        schedule.record_sent_frame(&chunk);
        assert!(!schedule.image_chunk_is_due());
    }

    #[test]
    fn rejects_chunks_from_a_different_transfer() {
        let image = sample_image();
        let mut incoming = IncomingClipboardTransfer::default();
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
                limits(1024),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 2,
                    offset: 0,
                    bytes: vec![1],
                },
                limits(1024)
            ),
            Err(ClipboardError::InvalidTransfer)
        ));
    }

    #[test]
    fn rejects_truncated_and_duplicated_transfers() {
        let image = sample_image();
        let start = ClipboardEvent::ImageStart {
            transfer_id: 1,
            sequence: 1,
            width: image.width,
            height: image.height,
            total_bytes: image.png.len() as u32,
            content_sha256: image.content_sha256,
        };

        // Ending early, with fewer bytes than promised, must not yield an image.
        let mut incoming = IncomingClipboardTransfer::default();
        incoming.handle(start.clone(), limits(1024)).unwrap();
        incoming
            .handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 1,
                    offset: 0,
                    bytes: image.png[..4].to_vec(),
                },
                limits(1024),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(ClipboardEvent::ImageEnd { transfer_id: 1 }, limits(1024)),
            Err(ClipboardError::InvalidTransfer)
        ));

        // Replaying a chunk that was already accepted is an offset mismatch.
        let mut incoming = IncomingClipboardTransfer::default();
        incoming.handle(start, limits(1024)).unwrap();
        let chunk = ClipboardEvent::ImageChunk {
            transfer_id: 1,
            offset: 0,
            bytes: image.png[..4].to_vec(),
        };
        incoming.handle(chunk.clone(), limits(1024)).unwrap();
        assert!(matches!(
            incoming.handle(chunk, limits(1024)),
            Err(ClipboardError::OffsetMismatch {
                expected: 4,
                actual: 0
            })
        ));
    }

    #[test]
    fn rejects_mismatched_hash_and_malformed_payload() {
        let image = sample_image();

        // Correct byte count and dimensions, but the promised hash is wrong.
        let mut incoming = IncomingClipboardTransfer::default();
        incoming
            .handle(
                ClipboardEvent::ImageStart {
                    transfer_id: 1,
                    sequence: 1,
                    width: image.width,
                    height: image.height,
                    total_bytes: image.png.len() as u32,
                    content_sha256: [0xAB; 32],
                },
                limits(4096),
            )
            .unwrap();
        incoming
            .handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 1,
                    offset: 0,
                    bytes: image.png.clone(),
                },
                limits(4096),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(ClipboardEvent::ImageEnd { transfer_id: 1 }, limits(4096)),
            Err(ClipboardError::HashMismatch)
        ));

        // Bytes that are not a decodable PNG at all.
        let garbage = vec![0x7F_u8; 32];
        let mut incoming = IncomingClipboardTransfer::default();
        incoming
            .handle(
                ClipboardEvent::ImageStart {
                    transfer_id: 2,
                    sequence: 2,
                    width: image.width,
                    height: image.height,
                    total_bytes: garbage.len() as u32,
                    content_sha256: image.content_sha256,
                },
                limits(4096),
            )
            .unwrap();
        incoming
            .handle(
                ClipboardEvent::ImageChunk {
                    transfer_id: 2,
                    offset: 0,
                    bytes: garbage,
                },
                limits(4096),
            )
            .unwrap();
        assert!(matches!(
            incoming.handle(ClipboardEvent::ImageEnd { transfer_id: 2 }, limits(4096)),
            Err(ClipboardError::Image(_))
        ));
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
