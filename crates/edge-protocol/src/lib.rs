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
    Input(InputFrame),
    Clipboard(ClipboardEvent),
    Control(ControlFrame),
    Heartbeat(Heartbeat),
    Error(RemoteError),
    Audio(AudioControl),
    Pairing(PairingEvent),
    Role(RoleEvent),
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
    ImageCancel {
        transfer_id: u64,
        reason: ClipboardCancelReason,
    },
}

impl ClipboardEvent {
    pub fn image_transfer_id(&self) -> Option<u64> {
        match self {
            Self::ImageStart { transfer_id, .. }
            | Self::ImageChunk { transfer_id, .. }
            | Self::ImageEnd { transfer_id }
            | Self::ImageCancel { transfer_id, .. } => Some(*transfer_id),
            Self::TextOffer { .. } | Self::TextRequest | Self::ContentRequest => None,
        }
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
            .chunks_exact(2)
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
}
