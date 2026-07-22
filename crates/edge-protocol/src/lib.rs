use edge_common::Role;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_PORT: u16 = 42_420;
pub const MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    Hello(Hello),
    ScreenInfo(ScreenInfo),
    Input(InputEvent),
    Clipboard(ClipboardEvent),
    Control(ControlEvent),
    Heartbeat(Heartbeat),
    Error(RemoteError),
    Audio(AudioControl),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub device_name: String,
    pub role: Role,
    pub public_key_fingerprint: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    AudioV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    PcmS16Stereo48Khz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioStreamState {
    Disabled,
    WaitingForUdp,
    Starting,
    Streaming,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioStopReason {
    UserRequest,
    PeerDisconnected,
    TransportFailure,
    CaptureFailure,
    PlaybackFailure,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioControl {
    Offer {
        udp_port: u16,
        codecs: Vec<AudioCodec>,
    },
    Start {
        udp_port: u16,
        session_id: [u8; 16],
        session_salt: [u8; 4],
        session_key: [u8; 32],
        codec: AudioCodec,
        frame_ms: u16,
        jitter_target_ms: u16,
    },
    SetEnabled {
        enabled: bool,
    },
    State {
        state: AudioStreamState,
        detail: Option<String>,
    },
    Stop {
        reason: AudioStopReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub outputs: Vec<OutputInfo>,
    pub primary_output: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMotion { dx: f64, dy: f64 },
    PointerButton { button: MouseButton, down: bool },
    PointerWheel { x: f64, y: f64 },
    Key { evdev_code: u16, down: bool },
    AllKeysUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardEvent {
    TextOffer { sequence: u64, text: String },
    TextRequest,
}

#[derive(Debug, Clone, Default)]
pub struct ClipboardChangeTracker {
    sequence: u64,
    last_observed_text: Option<String>,
}

impl ClipboardChangeTracker {
    pub fn new(last_observed_text: Option<String>) -> Self {
        Self {
            sequence: 0,
            last_observed_text,
        }
    }

    pub fn is_observed(&self, current: &Option<String>) -> bool {
        &self.last_observed_text == current
    }

    pub fn mark_observed(&mut self, current: Option<String>) {
        self.last_observed_text = current;
    }

    pub fn offer_if_changed(&mut self, current: Option<String>) -> Option<ClipboardEvent> {
        if self.is_observed(&current) {
            return None;
        }

        self.offer_current(current)
    }

    pub fn offer_current(&mut self, current: Option<String>) -> Option<ClipboardEvent> {
        self.last_observed_text = current.clone();
        let text = current?;
        self.sequence = self.sequence.saturating_add(1);
        Some(ClipboardEvent::TextOffer {
            sequence: self.sequence,
            text,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlEvent {
    EnterRemote { edge: Edge, normalized_y: f32 },
    LeaveRemote { edge: Edge, normalized_y: f32 },
    ReleaseToLocal { reason: ReleaseReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseReason {
    Hotkey,
    PeerDisconnected,
    HeartbeatTimeout,
    BackendFailure,
    UserRequest,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(frame).map_err(ProtocolError::from)
}

pub fn decode_frame(bytes: &[u8]) -> Result<Frame> {
    rmp_serde::from_slice(bytes).map_err(ProtocolError::from)
}

pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode_frame(frame)?;
    let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    writer.write_u32(len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await?;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut payload = vec![0; len as usize];
    reader.read_exact(&mut payload).await?;
    decode_frame(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messagepack_round_trip() {
        let frame = Frame::Input(InputEvent::Key {
            evdev_code: 30,
            down: true,
        });

        let encoded = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&encoded).unwrap();

        assert_eq!(decoded, frame);
    }

    #[test]
    fn audio_control_round_trip() {
        let frame = Frame::Audio(AudioControl::Start {
            udp_port: 42_421,
            session_id: [7; 16],
            session_salt: [8; 4],
            session_key: [9; 32],
            codec: AudioCodec::PcmS16Stereo48Khz,
            frame_ms: 5,
            jitter_target_ms: 60,
        });
        assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
    }

    #[test]
    fn clipboard_tracker_only_offers_changed_text() {
        let mut tracker = ClipboardChangeTracker::new(Some("initial".to_string()));

        assert_eq!(tracker.offer_if_changed(Some("initial".to_string())), None);
        assert_eq!(
            tracker.offer_if_changed(Some("next".to_string())),
            Some(ClipboardEvent::TextOffer {
                sequence: 1,
                text: "next".to_string(),
            })
        );
        assert_eq!(tracker.offer_if_changed(Some("next".to_string())), None);
    }

    #[test]
    fn clipboard_tracker_suppresses_remote_write_echo() {
        let mut tracker = ClipboardChangeTracker::new(Some("local".to_string()));

        tracker.mark_observed(Some("remote".to_string()));

        assert_eq!(tracker.offer_if_changed(Some("remote".to_string())), None);
    }

    #[test]
    fn clipboard_tracker_reset_allows_identical_text_to_be_copied_again() {
        let mut tracker = ClipboardChangeTracker::new(Some("repeat".to_string()));

        assert_eq!(tracker.offer_if_changed(None), None);
        assert_eq!(
            tracker.offer_if_changed(Some("repeat".to_string())),
            Some(ClipboardEvent::TextOffer {
                sequence: 1,
                text: "repeat".to_string(),
            })
        );
    }

    #[test]
    fn clipboard_event_round_trip_preserves_multiline_unicode() {
        let frame = Frame::Clipboard(ClipboardEvent::TextOffer {
            sequence: 42,
            text: "first line\nemoji: ✨\n日本語".to_string(),
        });

        assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
    }
}
