use edge_common::Role;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 2;
pub const INITIAL_ROLE_EPOCH: u64 = 1;
pub const DEFAULT_PORT: u16 = 42_420;
pub const MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;
pub const CLIPBOARD_IMAGE_EXTENSION: &str = "clipboard-image-v1";
pub const INPUT_TOGGLE_EXTENSION: &str = "input-toggle-v1";
pub const PAIRING_CONFIRMATION_EXTENSION: &str = "pairing-confirmation-v1";
pub const AUDIO_ROUTE_EXTENSION: &str = "audio-route-v1";
/// Peers advertising this extension accept large clipboard text as a chunked
/// `TextStart`/`TextChunk`/`TextEnd` transfer instead of a single `TextOffer`.
pub const CLIPBOARD_TEXT_CHUNKS_EXTENSION: &str = "clipboard-text-chunks-v1";
/// Largest serialized frame that fits in one encrypted transport message.
/// Noise caps a transport message at 65,535 bytes, including its 16-byte tag.
pub const MAX_SECURE_FRAME_BYTES: usize = 65_535 - 16;
/// Sent only by HyprMobile phones. A peer advertising it is handled as a
/// phone: restricted frames, phone liveness, and phone pins.
pub const HYPR_PHONE_EXTENSION: &str = "hypr-phone-v1";
/// A phone that mirrors its notifications; the receiver echoes it back when it
/// shows them.
pub const NOTIFICATIONS_EXTENSION: &str = "notifications-v1";
/// Shared desktop notification state; independent of KVM input roles.
pub const NOTIFICATION_SYNC_EXTENSION: &str = "notification-sync-v2";
/// A phone that accepts [`PhoneEvent::AudioInvite`], so the computer can ask
/// it to start a stream.
pub const PHONE_AUDIO_INVITE_EXTENSION: &str = "phone-audio-invite-v1";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error(transparent)]
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
    Input(InputFrame),
    Clipboard(ClipboardEvent),
    Control(ControlFrame),
    Heartbeat(Heartbeat),
    Error(RemoteError),
    Audio(AudioControl),
    Pairing(PairingEvent),
    Role(RoleEvent),
    /// Only exchanged with peers that advertised `hypr-phone-v1`.
    Phone(PhoneEvent),
    /// Phone → receiver, only with `notifications-v1`.
    Notification(NotificationEvent),
    /// Only exchanged by explicitly enrolled PCs advertising notification-sync-v1.
    NotificationSync(NotificationSync),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NotificationKey {
    pub origin: String,
    pub instance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedNotification {
    pub app_name: String,
    pub title: String,
    pub body: String,
    /// Theme icon name only, never a remote filesystem path.
    pub icon: String,
    /// Bounded phone icon bytes; receivers create their own local cache path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<Vec<u8>>,
    pub posted_ms: u64,
    pub urgency: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationOperation {
    pub sequence: u64,
    pub key: NotificationKey,
    /// None is a terminal removal. A removed instance cannot be posted again.
    pub content: Option<SharedNotification>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub key: NotificationKey,
    pub source_sequence: u64,
    pub content: Option<SharedNotification>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationSync {
    Submit(NotificationOperation),
    Ack { sequence: u64 },
    SnapshotStart { epoch: String },
    Record(NotificationRecord),
    SnapshotEnd,
    Settings { coordinator: String },
}

impl Frame {
    pub fn input(role_epoch: u64, event: InputEvent) -> Self {
        Self::Input(InputFrame { role_epoch, event })
    }

    pub fn control(role_epoch: u64, event: ControlEvent) -> Self {
        Self::Control(ControlFrame { role_epoch, event })
    }

    pub fn input_event(&self) -> Option<&InputEvent> {
        match self {
            Self::Input(input) => Some(&input.event),
            _ => None,
        }
    }

    pub fn control_event(&self) -> Option<&ControlEvent> {
        match self {
            Self::Control(control) => Some(&control.event),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingEvent {
    Status { trusted: bool, armed: bool },
    Decision { accepted: bool },
}

/// Phone-only messages. Phones pair with a one-time token from the
/// receiver's QR code instead of the two-sided PC confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhoneEvent {
    /// Phone → receiver, as the first frame after the receiver's `Hello`.
    /// The phone's name comes from its `Hello`.
    PairRequest { token: String },
    /// Receiver → phone: the phone is pinned. Also sent when an already
    /// pinned phone repeats its request, so a lost reply is recovered by
    /// asking again.
    PairAccepted,
    /// Phone → receiver: forget this phone. The receiver removes the pin and
    /// closes the session.
    Unpair,
    /// Phone → receiver: stream audio this way (stopping the other direction),
    /// or stop with `None`. `udp_port` is the phone's audio socket; the
    /// receiver sends to it first so the path works through its firewall.
    AudioRequest {
        direction: Option<AudioDirection>,
        udp_port: u16,
    },
    /// Receiver → phone: keys for the requested stream. Encrypted UDP packets
    /// use the same format as PC audio (`edge-audio`): 5 ms of 48 kHz stereo
    /// 16-bit PCM each.
    AudioStart {
        direction: AudioDirection,
        session_id: [u8; 16],
        session_salt: [u8; 4],
        session_key: [u8; 32],
        jitter_target_ms: u16,
    },
    /// Either way: the stream ended, and why.
    AudioStopped { reason: String },
    /// Receiver → phone: please start a stream this way by sending an
    /// `AudioRequest`, which keeps the phone's firewall path working. Only
    /// sent to phones advertising [`PHONE_AUDIO_INVITE_EXTENSION`].
    AudioInvite { direction: AudioDirection },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDirection {
    /// The computer's audio plays on the phone.
    ToPhone,
    /// The phone's media audio plays on the computer.
    FromPhone,
}

/// Mirrors the phone's current notifications (view only). After every
/// (re)connect the phone sends `SnapshotStart`, `Posted` for each current
/// notification, then `SnapshotEnd`; the receiver closes anything it showed
/// that wasn't posted again, so missed removals are fixed and unchanged ones
/// aren't shown twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationEvent {
    /// New, or an update to the notification with the same `id`.
    Posted(PhoneNotification),
    Removed {
        id: u64,
    },
    SnapshotStart,
    SnapshotEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneNotification {
    /// Stable for one notification on the phone across updates.
    pub id: u64,
    pub app_name: String,
    pub title: String,
    pub body: String,
    /// A small PNG (the app's icon or the notification's picture).
    pub icon_png: Option<Vec<u8>>,
    /// When the phone posted it, Unix milliseconds.
    pub posted_ms: u64,
}

impl PhoneNotification {
    pub const MAX_APP_NAME_CHARS: usize = 64;
    pub const MAX_TITLE_CHARS: usize = 256;
    pub const MAX_BODY_BYTES: usize = 4096;
    /// MessagePack stores each byte of a `Vec<u8>` as up to two bytes, so
    /// this keeps a notification well inside one 64 KiB Noise frame.
    pub const MAX_ICON_BYTES: usize = 24 * 1024;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub device_name: String,
    pub role: Role,
    pub public_key_fingerprint: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub node_capabilities: Vec<NodeCapability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    AudioV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeCapability {
    InputCaptureV1,
    InputInjectV1,
    RoleSwitchV1,
    ScreenInfoBothSidesV1,
    AudioCaptureV1,
    AudioPlaybackV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFrame {
    pub role_epoch: u64,
    pub event: InputEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlFrame {
    pub role_epoch: u64,
    pub event: ControlEvent,
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
    /// Listener asks the connector to commit a route. `None` disables audio.
    RequestRoute {
        source_fingerprint: Option<String>,
    },
    /// Connector-authoritative committed route. `None` disables audio.
    SetRoute {
        source_fingerprint: Option<String>,
    },
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
    TextOffer {
        sequence: u64,
        text: String,
    },
    TextRequest,
    ContentRequest,
    ImageStart {
        transfer_id: u64,
        sequence: u64,
        width: u32,
        height: u32,
        total_bytes: u32,
        content_sha256: [u8; 32],
    },
    ImageChunk {
        transfer_id: u64,
        offset: u32,
        bytes: Vec<u8>,
    },
    ImageEnd {
        transfer_id: u64,
    },
    /// Cancels any chunked clipboard transfer, image or text, by transfer id.
    /// The name is kept for wire compatibility with image-only peers.
    ImageCancel {
        transfer_id: u64,
        reason: ClipboardCancelReason,
    },
    /// Starts a chunked UTF-8 text transfer. Only sent to peers that
    /// advertise `CLIPBOARD_TEXT_CHUNKS_EXTENSION`.
    TextStart {
        transfer_id: u64,
        sequence: u64,
        total_bytes: u32,
        content_sha256: [u8; 32],
    },
    TextChunk {
        transfer_id: u64,
        offset: u32,
        bytes: Vec<u8>,
    },
    TextEnd {
        transfer_id: u64,
    },
}

impl ClipboardEvent {
    pub fn image_transfer_id(&self) -> Option<u64> {
        match self {
            Self::ImageStart { transfer_id, .. }
            | Self::ImageChunk { transfer_id, .. }
            | Self::ImageEnd { transfer_id }
            | Self::ImageCancel { transfer_id, .. } => Some(*transfer_id),
            Self::TextStart { .. }
            | Self::TextChunk { .. }
            | Self::TextEnd { .. }
            | Self::TextOffer { .. }
            | Self::TextRequest
            | Self::ContentRequest => None,
        }
    }

    /// Transfer id of any chunked clipboard transfer frame, image or text.
    pub fn transfer_id(&self) -> Option<u64> {
        match self {
            Self::TextStart { transfer_id, .. }
            | Self::TextChunk { transfer_id, .. }
            | Self::TextEnd { transfer_id } => Some(*transfer_id),
            _ => self.image_transfer_id(),
        }
    }

    /// Whether this frame belongs to a chunked text transfer.
    pub fn is_text_transfer(&self) -> bool {
        matches!(
            self,
            Self::TextStart { .. } | Self::TextChunk { .. } | Self::TextEnd { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardCancelReason {
    Replaced,
    Rejected,
    TimedOut,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlEvent {
    EnterRemote {
        edge: Edge,
        normalized_position: f32,
    },
    LeaveRemote {
        edge: Edge,
        normalized_position: f32,
    },
    ReleaseToLocal {
        reason: ReleaseReason,
    },
    SetInputForwarding {
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleTransitionState {
    Stable,
    Preparing {
        proposed_controller_fingerprint: String,
    },
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleState {
    pub controller_fingerprint: Option<String>,
    pub role_epoch: u64,
    pub transition: RoleTransitionState,
    pub listener_position: Edge,
    pub paused: bool,
    pub failure_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleEvent {
    SessionState(RoleState),
    Request {
        controller_fingerprint: String,
    },
    Prepare(RoleState),
    Ready {
        role_epoch: u64,
        capture_ready: bool,
        inject_ready: bool,
        failure_detail: Option<String>,
    },
    Commit(RoleState),
    Applied {
        role_epoch: u64,
    },
    Abort(RoleState),
    SetPaused {
        paused: bool,
    },
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

/// Measures the round trip between PC peers over their regular heartbeats.
///
/// A heartbeat with [`Self::ECHO_REQUEST`] set asks the peer to send it back
/// with [`Self::ECHO_REPLY`] instead. Builds without this ignore both bits,
/// as heartbeat sequences are only ever used for liveness, so against them
/// the round trip just stays unknown. Only one heartbeat every
/// [`Self::INTERVAL`] asks, and the round trip is the average of the last
/// few, so a single slow packet doesn't jump the number around.
#[derive(Debug, Default)]
pub struct HeartbeatPing {
    pending: Option<(u64, std::time::Instant)>,
    last_asked: Option<std::time::Instant>,
    samples: std::collections::VecDeque<std::time::Duration>,
}

impl HeartbeatPing {
    pub const ECHO_REQUEST: u64 = 1 << 62;
    pub const ECHO_REPLY: u64 = 1 << 61;
    const SEQUENCE: u64 = !(Self::ECHO_REQUEST | Self::ECHO_REPLY);
    /// How often a heartbeat asks for an echo.
    pub const INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    /// Samples averaged, so about ten seconds' worth.
    const SAMPLES: usize = 5;
    /// An echo this late is treated as lost and the next heartbeat asks again.
    const ECHO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// The heartbeat to send for `sequence`. It asks for an echo unless one
    /// is still on its way.
    pub fn outgoing(&mut self, sequence: u64, now: std::time::Instant) -> Heartbeat {
        let sequence = sequence & Self::SEQUENCE;
        let waiting = self
            .pending
            .is_some_and(|(_, sent)| now.duration_since(sent) < Self::ECHO_TIMEOUT);
        let too_soon = self
            .last_asked
            .is_some_and(|asked| now.duration_since(asked) < Self::INTERVAL);
        if waiting || too_soon {
            return Heartbeat { sequence };
        }
        self.pending = Some((sequence, now));
        self.last_asked = Some(now);
        Heartbeat {
            sequence: sequence | Self::ECHO_REQUEST,
        }
    }

    /// Handles a received heartbeat, returning the echo to send back if it
    /// asked for one. Returns `None` for echoes, which only update
    /// [`Self::round_trip`].
    pub fn incoming(
        &mut self,
        heartbeat: &Heartbeat,
        now: std::time::Instant,
    ) -> Option<Heartbeat> {
        let sequence = heartbeat.sequence & Self::SEQUENCE;
        if heartbeat.sequence & Self::ECHO_REPLY != 0 {
            if let Some((pending, sent)) = self.pending
                && pending == sequence
            {
                if self.samples.len() == Self::SAMPLES {
                    self.samples.pop_front();
                }
                self.samples.push_back(now.duration_since(sent));
                self.pending = None;
            }
            return None;
        }
        (heartbeat.sequence & Self::ECHO_REQUEST != 0).then_some(Heartbeat {
            sequence: sequence | Self::ECHO_REPLY,
        })
    }

    /// The average of the latest round trips, if the peer echoes heartbeats.
    pub fn round_trip(&self) -> Option<std::time::Duration> {
        let count = u32::try_from(self.samples.len())
            .ok()
            .filter(|count| *count > 0)?;
        Some(self.samples.iter().sum::<std::time::Duration>() / count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(frame).map_err(ProtocolError::from)
}

/// Returns whether `frame` can be sent as a single encrypted transport message.
pub fn fits_secure_frame(frame: &Frame) -> bool {
    encode_frame(frame).is_ok_and(|payload| payload.len() <= MAX_SECURE_FRAME_BYTES)
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
    use serde::{Deserialize, Serialize};

    #[test]
    fn messagepack_round_trip() {
        let frame = Frame::input(
            INITIAL_ROLE_EPOCH,
            InputEvent::Key {
                evdev_code: 30,
                down: true,
            },
        );

        let encoded = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&encoded).unwrap();

        assert_eq!(decoded, frame);
    }

    #[test]
    fn text_transfer_frames_round_trip_and_report_transfer_ids() {
        let frames = [
            ClipboardEvent::TextStart {
                transfer_id: 9,
                sequence: 4,
                total_bytes: 70_000,
                content_sha256: [3; 32],
            },
            ClipboardEvent::TextChunk {
                transfer_id: 9,
                offset: 0,
                bytes: vec![b'a'; 16],
            },
            ClipboardEvent::TextEnd { transfer_id: 9 },
        ];
        for event in frames {
            assert!(event.is_text_transfer());
            assert_eq!(event.transfer_id(), Some(9));
            assert_eq!(event.image_transfer_id(), None);
            let frame = Frame::Clipboard(event);
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn secure_frame_limit_matches_noise_message_size() {
        let fits = Frame::Clipboard(ClipboardEvent::TextOffer {
            sequence: 1,
            text: "a".repeat(MAX_SECURE_FRAME_BYTES - 64),
        });
        assert!(fits_secure_frame(&fits));
        let oversized = Frame::Clipboard(ClipboardEvent::TextOffer {
            sequence: 1,
            text: "a".repeat(MAX_SECURE_FRAME_BYTES),
        });
        assert!(!fits_secure_frame(&oversized));
    }

    #[test]
    fn audio_control_round_trip() {
        let start = Frame::Audio(AudioControl::Start {
            udp_port: 42_421,
            session_id: [7; 16],
            session_salt: [8; 4],
            session_key: [9; 32],
            codec: AudioCodec::PcmS16Stereo48Khz,
            frame_ms: 5,
            jitter_target_ms: 60,
        });
        assert_eq!(decode_frame(&encode_frame(&start).unwrap()).unwrap(), start);
        for source_fingerprint in [None, Some("source-fingerprint".to_string())] {
            let route = Frame::Audio(AudioControl::SetRoute { source_fingerprint });
            assert_eq!(decode_frame(&encode_frame(&route).unwrap()).unwrap(), route);
        }
    }

    #[test]
    fn input_forwarding_control_round_trip() {
        for enabled in [false, true] {
            let frame = Frame::control(
                INITIAL_ROLE_EPOCH,
                ControlEvent::SetInputForwarding { enabled },
            );
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn pairing_events_round_trip() {
        for event in [
            PairingEvent::Status {
                trusted: false,
                armed: true,
            },
            PairingEvent::Decision { accepted: true },
        ] {
            let frame = Frame::Pairing(event);
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn clipboard_event_round_trip_preserves_multiline_unicode() {
        let frame = Frame::Clipboard(ClipboardEvent::TextOffer {
            sequence: 42,
            text: "first line\nemoji: ✨\n日本語".to_string(),
        });

        assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
    }

    #[test]
    fn image_clipboard_events_round_trip_and_fit_secure_packets() {
        let events = [
            ClipboardEvent::ImageStart {
                transfer_id: 7,
                sequence: 3,
                width: 1920,
                height: 1080,
                total_bytes: 32_768,
                content_sha256: [9; 32],
            },
            ClipboardEvent::ImageChunk {
                transfer_id: 7,
                offset: 0,
                bytes: vec![4; 16 * 1024],
            },
            ClipboardEvent::ImageEnd { transfer_id: 7 },
            ClipboardEvent::ImageCancel {
                transfer_id: 7,
                reason: ClipboardCancelReason::Replaced,
            },
        ];
        for event in events {
            let frame = Frame::Clipboard(event);
            let encoded = encode_frame(&frame).unwrap();
            assert!(encoded.len() < 60 * 1024);
            assert_eq!(decode_frame(&encoded).unwrap(), frame);
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyHello {
        protocol_version: u16,
        device_name: String,
        role: Role,
        public_key_fingerprint: String,
        capabilities: Vec<Capability>,
    }

    #[derive(Debug, Deserialize)]
    enum LegacyHelloFrame {
        Hello(LegacyHello),
    }

    #[test]
    fn hello_extensions_are_backward_compatible() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "new".to_string(),
            role: Role::Controller,
            public_key_fingerprint: "fingerprint".to_string(),
            capabilities: vec![Capability::AudioV1],
            extensions: vec![
                CLIPBOARD_IMAGE_EXTENSION.to_string(),
                PAIRING_CONFIRMATION_EXTENSION.to_string(),
            ],
            node_capabilities: vec![NodeCapability::InputCaptureV1],
        };
        let encoded = rmp_serde::to_vec_named(&hello).unwrap();
        let legacy: LegacyHello = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(legacy.device_name, "new");

        let encoded_legacy = rmp_serde::to_vec_named(&legacy).unwrap();
        let decoded: Hello = rmp_serde::from_slice(&encoded_legacy).unwrap();
        assert!(decoded.extensions.is_empty());
    }

    #[test]
    fn prospective_v2_hello_reaches_version_check_in_v1_decoder() {
        let encoded = encode_frame(&Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "future-peer".to_string(),
            role: Role::Controller,
            public_key_fingerprint: "future-fingerprint".to_string(),
            // A v2 hello deliberately keeps this v1-known enum free of new
            // variants so an old peer can diagnose the version mismatch.
            capabilities: Vec::new(),
            extensions: Vec::new(),
            node_capabilities: vec![
                NodeCapability::InputCaptureV1,
                NodeCapability::InputInjectV1,
            ],
        }))
        .unwrap();

        let LegacyHelloFrame::Hello(legacy) = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(legacy.protocol_version, PROTOCOL_VERSION);
        assert!(legacy.capabilities.is_empty());
    }

    #[test]
    fn v2_role_messages_and_epoch_frames_round_trip() {
        let state = RoleState {
            controller_fingerprint: Some("connector-fingerprint".to_string()),
            role_epoch: 7,
            transition: RoleTransitionState::Preparing {
                proposed_controller_fingerprint: "listener-fingerprint".to_string(),
            },
            listener_position: Edge::Left,
            paused: false,
            failure_detail: None,
        };
        let frames = [
            Frame::Role(RoleEvent::Prepare(state.clone())),
            Frame::Role(RoleEvent::Ready {
                role_epoch: 7,
                capture_ready: true,
                inject_ready: true,
                failure_detail: None,
            }),
            Frame::Role(RoleEvent::Commit(RoleState {
                transition: RoleTransitionState::Stable,
                ..state
            })),
            Frame::Role(RoleEvent::Applied { role_epoch: 7 }),
            Frame::input(7, InputEvent::AllKeysUp),
            Frame::control(
                7,
                ControlEvent::EnterRemote {
                    edge: Edge::Left,
                    normalized_position: 0.25,
                },
            ),
        ];

        for frame in frames {
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    fn decode_hex_fixture(source: &str) -> Vec<u8> {
        let compact = source.split_whitespace().collect::<String>();
        assert_eq!(compact.len() % 2, 0, "hex fixture must contain byte pairs");
        compact
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn protocol_v1_wire_fixtures_remain_stable() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum V1Frame {
            Hello(V1Hello),
            Control(V1ControlEvent),
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct V1Hello {
            protocol_version: u16,
            device_name: String,
            role: Role,
            public_key_fingerprint: String,
            capabilities: Vec<Capability>,
            extensions: Vec<String>,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum V1ControlEvent {
            EnterRemote { edge: Edge, normalized_y: f32 },
            SetInputForwarding { enabled: bool },
        }

        let fixtures = [
            (
                V1Frame::Hello(V1Hello {
                    protocol_version: 1,
                    device_name: "fixture-peer".to_string(),
                    role: Role::Controller,
                    public_key_fingerprint: "fixture-fingerprint".to_string(),
                    capabilities: vec![Capability::AudioV1],
                    extensions: vec![INPUT_TOGGLE_EXTENSION.to_string()],
                }),
                include_str!("../fixtures/v1-hello.hex"),
            ),
            (
                V1Frame::Control(V1ControlEvent::EnterRemote {
                    edge: Edge::Left,
                    normalized_y: 0.25,
                }),
                include_str!("../fixtures/v1-enter-remote.hex"),
            ),
            (
                V1Frame::Control(V1ControlEvent::SetInputForwarding { enabled: false }),
                include_str!("../fixtures/v1-input-forwarding.hex"),
            ),
        ];

        for (expected, fixture) in fixtures {
            let fixture = decode_hex_fixture(fixture);
            assert_eq!(
                rmp_serde::from_slice::<V1Frame>(&fixture).unwrap(),
                expected
            );
            assert_eq!(rmp_serde::to_vec_named(&expected).unwrap(), fixture);
        }
    }

    /// Frames exchanged by the protocol v2 Windows and Linux builds (`main`
    /// 86fa623), paired with their frozen encodings. Multi-peer and phone
    /// support must not change any of these bytes.
    fn v2_wire_fixtures() -> Vec<(Frame, &'static str)> {
        let role_state = RoleState {
            controller_fingerprint: Some("controller-fingerprint".to_string()),
            role_epoch: 3,
            transition: RoleTransitionState::Preparing {
                proposed_controller_fingerprint: "receiver-fingerprint".to_string(),
            },
            listener_position: Edge::Right,
            paused: false,
            failure_detail: None,
        };
        vec![
            (
                Frame::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    device_name: "fixture-receiver".to_string(),
                    role: Role::Receiver,
                    public_key_fingerprint: "receiver-fingerprint".to_string(),
                    capabilities: Vec::new(),
                    extensions: vec![
                        CLIPBOARD_IMAGE_EXTENSION.to_string(),
                        INPUT_TOGGLE_EXTENSION.to_string(),
                        PAIRING_CONFIRMATION_EXTENSION.to_string(),
                        AUDIO_ROUTE_EXTENSION.to_string(),
                    ],
                    node_capabilities: vec![
                        NodeCapability::InputCaptureV1,
                        NodeCapability::InputInjectV1,
                        NodeCapability::RoleSwitchV1,
                        NodeCapability::ScreenInfoBothSidesV1,
                        NodeCapability::AudioCaptureV1,
                        NodeCapability::AudioPlaybackV1,
                    ],
                }),
                include_str!("../fixtures/v2-hello.hex"),
            ),
            (
                Frame::Pairing(PairingEvent::Status {
                    trusted: false,
                    armed: true,
                }),
                include_str!("../fixtures/v2-pairing-status.hex"),
            ),
            (
                Frame::Pairing(PairingEvent::Decision { accepted: true }),
                include_str!("../fixtures/v2-pairing-decision.hex"),
            ),
            (
                Frame::ScreenInfo(ScreenInfo {
                    outputs: vec![OutputInfo {
                        name: "eDP-1".to_string(),
                        width: 2560,
                        height: 1600,
                        scale: 1.5,
                        x: 0,
                        y: -10,
                    }],
                    primary_output: "eDP-1".to_string(),
                }),
                include_str!("../fixtures/v2-screen-info.hex"),
            ),
            (
                Frame::input(3, InputEvent::PointerMotion { dx: 1.5, dy: -2.25 }),
                include_str!("../fixtures/v2-input-motion.hex"),
            ),
            (
                Frame::input(
                    3,
                    InputEvent::Key {
                        evdev_code: 30,
                        down: true,
                    },
                ),
                include_str!("../fixtures/v2-input-key.hex"),
            ),
            (
                Frame::control(
                    3,
                    ControlEvent::ReleaseToLocal {
                        reason: ReleaseReason::HeartbeatTimeout,
                    },
                ),
                include_str!("../fixtures/v2-control-release.hex"),
            ),
            (
                Frame::Clipboard(ClipboardEvent::TextOffer {
                    sequence: 42,
                    text: "line one\nemoji: ✨".to_string(),
                }),
                include_str!("../fixtures/v2-clipboard-text.hex"),
            ),
            (
                Frame::Clipboard(ClipboardEvent::ImageStart {
                    transfer_id: 7,
                    sequence: 43,
                    width: 2,
                    height: 1,
                    total_bytes: 8,
                    content_sha256: [9; 32],
                }),
                include_str!("../fixtures/v2-clipboard-image-start.hex"),
            ),
            (
                Frame::Clipboard(ClipboardEvent::TextStart {
                    transfer_id: 8,
                    sequence: 44,
                    total_bytes: 5,
                    content_sha256: [10; 32],
                }),
                include_str!("../fixtures/v2-clipboard-text-start.hex"),
            ),
            (
                Frame::Clipboard(ClipboardEvent::TextChunk {
                    transfer_id: 8,
                    offset: 0,
                    bytes: b"hello".to_vec(),
                }),
                include_str!("../fixtures/v2-clipboard-text-chunk.hex"),
            ),
            (
                Frame::Clipboard(ClipboardEvent::TextEnd { transfer_id: 8 }),
                include_str!("../fixtures/v2-clipboard-text-end.hex"),
            ),
            (
                Frame::Heartbeat(Heartbeat { sequence: 99 }),
                include_str!("../fixtures/v2-heartbeat.hex"),
            ),
            (
                Frame::Error(RemoteError {
                    code: "pairing_not_armed".to_string(),
                    message: "fixture message".to_string(),
                }),
                include_str!("../fixtures/v2-error.hex"),
            ),
            (
                Frame::Audio(AudioControl::Offer {
                    udp_port: 42_421,
                    codecs: vec![AudioCodec::PcmS16Stereo48Khz],
                }),
                include_str!("../fixtures/v2-audio-offer.hex"),
            ),
            (
                Frame::Audio(AudioControl::Start {
                    udp_port: 42_421,
                    session_id: [7; 16],
                    session_salt: [8; 4],
                    session_key: [9; 32],
                    codec: AudioCodec::PcmS16Stereo48Khz,
                    frame_ms: 5,
                    jitter_target_ms: 60,
                }),
                include_str!("../fixtures/v2-audio-start.hex"),
            ),
            (
                Frame::Audio(AudioControl::SetRoute {
                    source_fingerprint: Some("receiver-fingerprint".to_string()),
                }),
                include_str!("../fixtures/v2-audio-route.hex"),
            ),
            (
                Frame::Role(RoleEvent::SessionState(role_state.clone())),
                include_str!("../fixtures/v2-role-state.hex"),
            ),
            (
                Frame::Role(RoleEvent::Ready {
                    role_epoch: 3,
                    capture_ready: true,
                    inject_ready: false,
                    failure_detail: Some("fixture detail".to_string()),
                }),
                include_str!("../fixtures/v2-role-ready.hex"),
            ),
        ]
    }

    #[test]
    fn the_largest_notification_fits_in_one_noise_frame() {
        let frame = Frame::Notification(NotificationEvent::Posted(PhoneNotification {
            id: u64::MAX,
            app_name: "\u{1F600}".repeat(PhoneNotification::MAX_APP_NAME_CHARS),
            title: "\u{1F600}".repeat(PhoneNotification::MAX_TITLE_CHARS),
            body: "a".repeat(PhoneNotification::MAX_BODY_BYTES),
            icon_png: Some(vec![0xff; PhoneNotification::MAX_ICON_BYTES]),
            posted_ms: u64::MAX,
        }));
        let encoded = encode_frame(&frame).unwrap();
        assert!(encoded.len() + 16 <= 65_535, "{} bytes", encoded.len());
        assert_eq!(decode_frame(&encoded).unwrap(), frame);
    }

    #[test]
    fn shared_notification_snapshots_fit_secure_frames_with_unicode() {
        let key = NotificationKey {
            origin: "01234567:89abcdef:01234567:89abcdef".into(),
            instance: "epoch/42".into(),
        };
        for event in [
            NotificationSync::SnapshotStart {
                epoch: "0".repeat(32),
            },
            NotificationSync::Record(NotificationRecord {
                key,
                source_sequence: u64::MAX,
                content: Some(SharedNotification {
                    app_name: "😀".repeat(64),
                    title: "😀".repeat(256),
                    body: "a".repeat(4096),
                    icon: "mail-unread".into(),
                    icon_png: Some(vec![0xff; PhoneNotification::MAX_ICON_BYTES]),
                    posted_ms: u64::MAX,
                    urgency: 2,
                }),
            }),
            NotificationSync::SnapshotEnd,
        ] {
            let frame = Frame::NotificationSync(event);
            let encoded = encode_frame(&frame).unwrap();
            assert!(encoded.len() + 16 <= 65_535);
            assert_eq!(decode_frame(&encoded).unwrap(), frame);
        }
    }

    #[test]
    fn phone_events_round_trip() {
        for event in [
            PhoneEvent::PairRequest {
                token: "0123456789abcdef0123456789abcdef".to_string(),
            },
            PhoneEvent::PairAccepted,
            PhoneEvent::Unpair,
            PhoneEvent::AudioRequest {
                direction: Some(AudioDirection::FromPhone),
                udp_port: 40_000,
            },
            PhoneEvent::AudioStart {
                direction: AudioDirection::ToPhone,
                session_id: [1; 16],
                session_salt: [2; 4],
                session_key: [3; 32],
                jitter_target_ms: 80,
            },
            PhoneEvent::AudioStopped {
                reason: "user".into(),
            },
            PhoneEvent::AudioInvite {
                direction: AudioDirection::ToPhone,
            },
        ] {
            let frame = Frame::Phone(event);
            assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn heartbeat_ping_measures_the_round_trip_of_echoed_heartbeats() {
        let start = std::time::Instant::now();
        let mut local = HeartbeatPing::default();
        let mut peer = HeartbeatPing::default();

        let ping = local.outgoing(7, start);
        assert_eq!(ping.sequence, 7 | HeartbeatPing::ECHO_REQUEST);
        // Heartbeats in between don't ask.
        let next = local.outgoing(8, start + std::time::Duration::from_millis(250));
        assert_eq!(next.sequence, 8);
        assert_eq!(peer.incoming(&next, start), None);

        let echo = peer.incoming(&ping, start).expect("peer echoes the ping");
        assert_eq!(echo.sequence, 7 | HeartbeatPing::ECHO_REPLY);
        assert_eq!(
            local.incoming(&echo, start + std::time::Duration::from_millis(4)),
            None
        );
        assert_eq!(
            local.round_trip(),
            Some(std::time::Duration::from_millis(4))
        );
    }

    #[test]
    fn heartbeat_ping_averages_recent_round_trips() {
        let mut now = std::time::Instant::now();
        let mut local = HeartbeatPing::default();
        let mut peer = HeartbeatPing::default();
        for (sequence, millis) in [(1, 2), (2, 4), (3, 12)] {
            let ping = local.outgoing(sequence, now);
            let echo = peer.incoming(&ping, now).unwrap();
            local.incoming(&echo, now + std::time::Duration::from_millis(millis));
            now += HeartbeatPing::INTERVAL;
        }
        assert_eq!(
            local.round_trip(),
            Some(std::time::Duration::from_millis(6))
        );
    }

    #[test]
    fn heartbeat_ping_asks_again_after_a_lost_echo() {
        let start = std::time::Instant::now();
        let mut local = HeartbeatPing::default();
        local.outgoing(1, start);
        let retry = local.outgoing(2, start + std::time::Duration::from_secs(6));
        assert_eq!(retry.sequence, 2 | HeartbeatPing::ECHO_REQUEST);
        // The late echo of the first ping is ignored.
        local.incoming(
            &Heartbeat {
                sequence: 1 | HeartbeatPing::ECHO_REPLY,
            },
            start + std::time::Duration::from_secs(7),
        );
        assert_eq!(local.round_trip(), None);
    }

    #[test]
    fn protocol_v2_wire_fixtures_remain_stable() {
        for (expected, fixture) in v2_wire_fixtures() {
            let fixture = decode_hex_fixture(fixture);
            assert_eq!(decode_frame(&fixture).unwrap(), expected);
            assert_eq!(encode_frame(&expected).unwrap(), fixture, "{expected:?}");
        }
    }
}
