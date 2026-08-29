use std::time::Duration;

use edge_clipboard::ImageTransferSchedule;
use edge_crypto::{CryptoError, NoiseReader, NoiseSession, NoiseWriter};
use edge_protocol::{
    ClipboardEvent, ControlEvent, Edge, Frame, InputEvent, ProtocolError, ScreenInfo, decode_frame,
    encode_frame,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf},
    time::Instant,
};

mod audio_route;
mod role;

pub use audio_route::{AudioRouteStore, CommittedAudioRoute};

pub use role::{
    CommittedRole, InputDirectionCapabilities, RoleCoordinator, RoleDecision, RoleStateError,
    RoleStore, select_initial_controller, validate_commit, validate_prepare,
};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

pub struct SecureFrameSession<S> {
    inner: NoiseSession<S>,
}

impl<S> SecureFrameSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(inner: NoiseSession<S>) -> Self {
        Self { inner }
    }

    pub async fn read(&mut self) -> Result<Frame> {
        let payload = self.inner.read_packet().await?;
        Ok(decode_frame(&payload)?)
    }

    pub async fn write(&mut self, frame: &Frame) -> Result<()> {
        let payload = encode_frame(frame)?;
        self.inner.write_packet(&payload).await?;
        Ok(())
    }

    pub fn split(
        self,
    ) -> (
        SecureFrameReader<ReadHalf<S>>,
        SecureFrameWriter<WriteHalf<S>>,
    ) {
        let (reader, writer) = self.inner.split();
        (
            SecureFrameReader { inner: reader },
            SecureFrameWriter {
                inner: writer,
                image_schedule: ImageTransferSchedule::default(),
            },
        )
    }
}

pub struct SecureFrameReader<R> {
    inner: NoiseReader<R>,
}

impl<R> SecureFrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub async fn read(&mut self) -> Result<Frame> {
        let payload = self.inner.read_packet().await?;
        Ok(decode_frame(&payload)?)
    }
}

pub struct SecureFrameWriter<W> {
    inner: NoiseWriter<W>,
    image_schedule: ImageTransferSchedule,
}

impl<W> SecureFrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub async fn write(&mut self, frame: &Frame) -> Result<()> {
        let payload = encode_frame(frame)?;
        self.inner.write_packet(&payload).await?;
        self.image_schedule.record_sent_frame(frame);
        Ok(())
    }

    pub fn bulk_is_due(&self) -> bool {
        self.image_schedule.image_chunk_is_due()
    }

    pub fn record_received(&mut self, frame: &Frame) {
        self.image_schedule.record_received_frame(frame);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessConfig {
    pub active_heartbeat_interval: Duration,
    pub idle_heartbeat_interval: Duration,
    pub soft_input_timeout: Duration,
    pub hard_session_timeout: Duration,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            active_heartbeat_interval: Duration::from_millis(250),
            idle_heartbeat_interval: Duration::from_secs(1),
            soft_input_timeout: Duration::from_secs(1),
            hard_session_timeout: Duration::from_secs(5),
        }
    }
}

impl LivenessConfig {
    pub fn heartbeat_interval(self, input_armed_or_transitioning: bool) -> Duration {
        if input_armed_or_transitioning {
            self.active_heartbeat_interval
        } else {
            self.idle_heartbeat_interval
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessEvent {
    SoftInputTimeout,
    HardSessionTimeout,
}

#[derive(Debug, Clone)]
pub struct LivenessTracker {
    config: LivenessConfig,
    last_authenticated_activity: Instant,
    input_active_since: Option<Instant>,
    soft_timeout_reported: bool,
    hard_timeout_reported: bool,
}

impl LivenessTracker {
    pub fn new(config: LivenessConfig, now: Instant) -> Self {
        assert!(
            config.soft_input_timeout < config.hard_session_timeout,
            "soft input timeout must precede hard session timeout"
        );
        Self {
            config,
            last_authenticated_activity: now,
            input_active_since: None,
            soft_timeout_reported: false,
            hard_timeout_reported: false,
        }
    }

    pub fn observe_authenticated_frame(&mut self, now: Instant) {
        self.last_authenticated_activity = now;
        self.soft_timeout_reported = false;
        self.hard_timeout_reported = false;
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_authenticated_activity)
    }

    pub fn poll(&mut self, now: Instant, input_active: bool) -> Option<LivenessEvent> {
        let elapsed = self.elapsed(now);
        if elapsed >= self.config.hard_session_timeout && !self.hard_timeout_reported {
            self.hard_timeout_reported = true;
            return Some(LivenessEvent::HardSessionTimeout);
        }

        if !input_active {
            self.input_active_since = None;
            self.soft_timeout_reported = false;
            return None;
        }

        let input_active_since = *self.input_active_since.get_or_insert(now);
        let soft_timeout_baseline = self.last_authenticated_activity.max(input_active_since);
        if now.saturating_duration_since(soft_timeout_baseline) >= self.config.soft_input_timeout
            && !self.soft_timeout_reported
        {
            self.soft_timeout_reported = true;
            return Some(LivenessEvent::SoftInputTimeout);
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct InputEpochGate {
    suspended: bool,
}

impl Default for InputEpochGate {
    fn default() -> Self {
        Self { suspended: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingStatus {
    pub trusted: bool,
    pub armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingResolution {
    AlreadyTrusted,
    ApprovalRequired,
    NotArmed,
    Declined,
    Accepted,
}

pub fn resolve_pairing(
    local: PairingStatus,
    peer: PairingStatus,
    decisions: Option<(bool, bool)>,
) -> PairingResolution {
    if local.trusted && peer.trusted {
        return PairingResolution::AlreadyTrusted;
    }
    if !local.armed || !peer.armed {
        return PairingResolution::NotArmed;
    }
    match decisions {
        None => PairingResolution::ApprovalRequired,
        Some((true, true)) => PairingResolution::Accepted,
        Some(_) => PairingResolution::Declined,
    }
}

impl InputEpochGate {
    pub fn suspend(&mut self) {
        self.suspended = true;
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn observe_control(&mut self, control: &ControlEvent) {
        if matches!(control, ControlEvent::EnterRemote { .. }) {
            self.suspended = false;
        }
    }

    pub fn accepts_input(&self) -> bool {
        !self.suspended
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePreparation {
    pub layout_generation: u64,
    pub exit_edge: Edge,
}

#[allow(async_fn_in_trait)]
pub trait InputCapture {
    type Error;

    async fn preflight(
        &mut self,
        layout: &ScreenInfo,
        exit_edge: Edge,
    ) -> std::result::Result<CapturePreparation, Self::Error>;
    async fn arm(
        &mut self,
        preparation: CapturePreparation,
    ) -> std::result::Result<(), Self::Error>;
    async fn disarm(&mut self) -> std::result::Result<(), Self::Error>;
}

#[allow(async_fn_in_trait)]
pub trait InputInjector {
    type Error;

    async fn inject(&mut self, event: InputEvent) -> std::result::Result<(), Self::Error>;
    async fn all_keys_up(&mut self) -> std::result::Result<(), Self::Error>;
}

#[allow(async_fn_in_trait)]
pub trait ClipboardAdapter {
    type Error;

    async fn handle_remote(
        &mut self,
        event: ClipboardEvent,
    ) -> std::result::Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use edge_common::Role;
    use edge_crypto::{
        IdentityKey, PinStatus, PinStore, accept_noise_session, initiate_noise_session,
    };
    use edge_protocol::{
        Heartbeat, Hello, INITIAL_ROLE_EPOCH, MouseButton, OutputInfo, PROTOCOL_VERSION,
    };

    #[derive(Default)]
    struct FakeCapture {
        armed: bool,
        disarms: usize,
    }

    impl InputCapture for FakeCapture {
        type Error = std::convert::Infallible;

        async fn preflight(
            &mut self,
            _layout: &ScreenInfo,
            exit_edge: Edge,
        ) -> std::result::Result<CapturePreparation, Self::Error> {
            Ok(CapturePreparation {
                layout_generation: 1,
                exit_edge,
            })
        }

        async fn arm(
            &mut self,
            _preparation: CapturePreparation,
        ) -> std::result::Result<(), Self::Error> {
            self.armed = true;
            Ok(())
        }

        async fn disarm(&mut self) -> std::result::Result<(), Self::Error> {
            self.armed = false;
            self.disarms += 1;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeInjector {
        events: Vec<InputEvent>,
    }

    impl InputInjector for FakeInjector {
        type Error = std::convert::Infallible;

        async fn inject(&mut self, event: InputEvent) -> std::result::Result<(), Self::Error> {
            self.events.push(event);
            Ok(())
        }

        async fn all_keys_up(&mut self) -> std::result::Result<(), Self::Error> {
            self.events.push(InputEvent::AllKeysUp);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClipboard {
        events: Vec<ClipboardEvent>,
    }

    impl ClipboardAdapter for FakeClipboard {
        type Error = std::convert::Infallible;

        async fn handle_remote(
            &mut self,
            event: ClipboardEvent,
        ) -> std::result::Result<(), Self::Error> {
            self.events.push(event);
            Ok(())
        }
    }

    fn test_screen_info() -> ScreenInfo {
        ScreenInfo {
            outputs: vec![OutputInfo {
                name: "test".to_string(),
                width: 1920,
                height: 1080,
                scale: 1.0,
                x: 0,
                y: 0,
            }],
            primary_output: "test".to_string(),
        }
    }

    #[test]
    fn heartbeat_rate_adapts_to_input_ownership() {
        let config = LivenessConfig::default();
        assert_eq!(config.heartbeat_interval(true), Duration::from_millis(250));
        assert_eq!(config.heartbeat_interval(false), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_soft_releases_before_hard_disconnect() {
        let start = Instant::now();
        let mut tracker = LivenessTracker::new(LivenessConfig::default(), start);

        assert_eq!(tracker.poll(Instant::now(), true), None);
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(tracker.poll(Instant::now(), true), None);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            tracker.poll(Instant::now(), true),
            Some(LivenessEvent::SoftInputTimeout)
        );
        assert_eq!(tracker.poll(Instant::now(), true), None);
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            tracker.poll(Instant::now(), true),
            Some(LivenessEvent::HardSessionTimeout)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_idle_time_does_not_arm_the_soft_input_timeout() {
        let start = Instant::now();
        let mut tracker = LivenessTracker::new(LivenessConfig::default(), start);

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(tracker.poll(Instant::now(), false), None);
        assert_eq!(tracker.poll(Instant::now(), true), None);

        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(tracker.poll(Instant::now(), true), None);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            tracker.poll(Instant::now(), true),
            Some(LivenessEvent::SoftInputTimeout)
        );
    }

    #[test]
    fn stale_input_is_rejected_until_a_fresh_entry() {
        let mut gate = InputEpochGate::default();
        assert!(!gate.accepts_input());
        gate.observe_control(&ControlEvent::LeaveRemote {
            edge: Edge::Right,
            normalized_position: 0.5,
        });
        assert!(!gate.accepts_input());
        gate.observe_control(&ControlEvent::EnterRemote {
            edge: Edge::Left,
            normalized_position: 0.5,
        });
        assert!(gate.accepts_input());
    }

    #[tokio::test]
    async fn fake_adapters_exercise_release_and_clipboard_seams() {
        let mut capture = FakeCapture::default();
        let preparation = capture
            .preflight(&test_screen_info(), Edge::Left)
            .await
            .unwrap();
        capture.arm(preparation).await.unwrap();
        capture.disarm().await.unwrap();

        let mut injector = FakeInjector::default();
        injector
            .inject(InputEvent::PointerButton {
                button: MouseButton::Left,
                down: true,
            })
            .await
            .unwrap();
        injector.all_keys_up().await.unwrap();

        let mut clipboard = FakeClipboard::default();
        clipboard
            .handle_remote(ClipboardEvent::TextOffer {
                sequence: 1,
                text: "fixture".to_string(),
            })
            .await
            .unwrap();

        assert!(!capture.armed);
        assert_eq!(capture.disarms, 1);
        assert_eq!(injector.events.last(), Some(&InputEvent::AllKeysUp));
        assert_eq!(clipboard.events.len(), 1);
    }

    #[tokio::test]
    async fn untrusted_pairing_uses_injected_decisions_and_persists_only_acceptance() {
        let local = PairingStatus {
            trusted: false,
            armed: true,
        };
        let peer = PairingStatus {
            trusted: false,
            armed: true,
        };
        let mut injected_decisions = VecDeque::from([(true, false), (true, true)]);

        assert_eq!(
            resolve_pairing(local, peer, None),
            PairingResolution::ApprovalRequired
        );
        assert_eq!(
            resolve_pairing(local, peer, injected_decisions.pop_front()),
            PairingResolution::Declined
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pins.toml");
        assert!(!path.exists());

        assert_eq!(
            resolve_pairing(local, peer, injected_decisions.pop_front()),
            PairingResolution::Accepted
        );
        let mut pins = PinStore::default();
        pins.pin("peer", "accepted-fingerprint");
        pins.save(&path).await.unwrap();
        let persisted = PinStore::load_or_default(&path).await.unwrap();
        assert_eq!(
            persisted.status("peer", "accepted-fingerprint"),
            PinStatus::Trusted
        );
    }

    #[tokio::test]
    async fn encrypted_duplex_harness_covers_shared_session_frames() {
        let connector_identity = IdentityKey::generate().unwrap();
        let listener_identity = IdentityKey::generate().unwrap();
        let expected_listener = listener_identity.fingerprint();
        let (connector_io, listener_io) = tokio::io::duplex(64 * 1024);

        let connector = tokio::spawn(async move {
            let (session, _) =
                initiate_noise_session(connector_io, &connector_identity, Some(&expected_listener))
                    .await
                    .unwrap();
            let mut session = SecureFrameSession::new(session);
            session
                .write(&Frame::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    device_name: "connector".to_string(),
                    role: Role::Controller,
                    public_key_fingerprint: connector_identity.fingerprint(),
                    capabilities: Vec::new(),
                    extensions: Vec::new(),
                    node_capabilities: Vec::new(),
                }))
                .await
                .unwrap();
            assert!(matches!(
                session.read().await.unwrap(),
                Frame::ScreenInfo(_)
            ));
            session
                .write(&Frame::control(
                    INITIAL_ROLE_EPOCH,
                    ControlEvent::EnterRemote {
                        edge: Edge::Left,
                        normalized_position: 0.5,
                    },
                ))
                .await
                .unwrap();
            session
                .write(&Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::Key {
                        evdev_code: 30,
                        down: true,
                    },
                ))
                .await
                .unwrap();
            session
                .write(&Frame::Clipboard(ClipboardEvent::TextOffer {
                    sequence: 1,
                    text: "loopback".to_string(),
                }))
                .await
                .unwrap();
            assert!(matches!(session.read().await.unwrap(), Frame::Heartbeat(_)));
            session
                .write(&Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp))
                .await
                .unwrap();
        });

        let listener = tokio::spawn(async move {
            let (session, _) = accept_noise_session(listener_io, &listener_identity)
                .await
                .unwrap();
            let mut session = SecureFrameSession::new(session);
            assert!(matches!(session.read().await.unwrap(), Frame::Hello(_)));
            session
                .write(&Frame::ScreenInfo(test_screen_info()))
                .await
                .unwrap();
            assert!(matches!(session.read().await.unwrap(), Frame::Control(_)));
            assert!(matches!(session.read().await.unwrap(), Frame::Input(_)));
            assert!(matches!(session.read().await.unwrap(), Frame::Clipboard(_)));
            session
                .write(&Frame::Heartbeat(Heartbeat { sequence: 1 }))
                .await
                .unwrap();
            assert_eq!(
                session.read().await.unwrap(),
                Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp)
            );
        });

        connector.await.unwrap();
        listener.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn throttled_four_mib_transfer_stays_below_soft_liveness_gap() {
        const CHUNK_BYTES: usize = 16 * 1024;
        const TOTAL_BYTES: usize = 4 * 1024 * 1024;
        const CHUNKS: usize = TOTAL_BYTES / CHUNK_BYTES;
        const HEARTBEAT_EVERY_CHUNKS: usize = 8;
        const EXPECTED_FRAMES: usize = 2 + CHUNKS + CHUNKS / HEARTBEAT_EVERY_CHUNKS;

        let connector_identity = IdentityKey::generate().unwrap();
        let listener_identity = IdentityKey::generate().unwrap();
        let expected_listener = listener_identity.fingerprint();
        // The buffer is deliberately much smaller than one image chunk, so a
        // sender cannot dump the transfer into memory and appear responsive.
        let (connector_io, listener_io) = tokio::io::duplex(1024);

        let sender = tokio::spawn(async move {
            let (session, _) =
                initiate_noise_session(connector_io, &connector_identity, Some(&expected_listener))
                    .await
                    .unwrap();
            let mut session = SecureFrameSession::new(session);
            session
                .write(&Frame::Clipboard(ClipboardEvent::ImageStart {
                    transfer_id: 7,
                    sequence: 1,
                    width: 1024,
                    height: 1024,
                    total_bytes: TOTAL_BYTES as u32,
                    content_sha256: [7; 32],
                }))
                .await
                .unwrap();
            for chunk in 0..CHUNKS {
                if chunk % HEARTBEAT_EVERY_CHUNKS == 0 {
                    session
                        .write(&Frame::Heartbeat(Heartbeat {
                            sequence: chunk as u64,
                        }))
                        .await
                        .unwrap();
                }
                session
                    .write(&Frame::Clipboard(ClipboardEvent::ImageChunk {
                        transfer_id: 7,
                        offset: (chunk * CHUNK_BYTES) as u32,
                        bytes: vec![chunk as u8; CHUNK_BYTES],
                    }))
                    .await
                    .unwrap();
            }
            session
                .write(&Frame::Clipboard(ClipboardEvent::ImageEnd {
                    transfer_id: 7,
                }))
                .await
                .unwrap();
        });

        let receiver = tokio::spawn(async move {
            let (session, _) = accept_noise_session(listener_io, &listener_identity)
                .await
                .unwrap();
            let mut session = SecureFrameSession::new(session);
            let mut previous = Instant::now();
            let mut maximum_gap = Duration::ZERO;
            for _ in 0..EXPECTED_FRAMES {
                session.read().await.unwrap();
                let now = Instant::now();
                maximum_gap = maximum_gap.max(now.saturating_duration_since(previous));
                previous = now;
                // Model a constrained consumer/link while allowing paused Tokio
                // time to keep the test fast and deterministic.
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
            maximum_gap
        });

        sender.await.unwrap();
        let maximum_gap = receiver.await.unwrap();
        eprintln!("maximum complete-frame gap during 4 MiB transfer: {maximum_gap:?}");
        assert!(maximum_gap < LivenessConfig::default().soft_input_timeout);
    }
}
