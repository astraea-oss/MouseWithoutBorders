//! Phone sessions.
//!
//! A peer advertising `hypr-phone-v1` never reaches the PC pairing flow or the
//! KVM session. It gets a small session of its own, running alongside any PC
//! session, with:
//!
//! - **Phone pins only.** The phone must be pinned in the pin store's phone
//!   table; a key pinned for a PC is refused.
//! - **A frame allowlist.** Input, control, screen, role and pairing frames
//!   close the session.
//! - **Laptop-driven liveness.** A sleeping phone's timers stop but incoming
//!   packets wake it, so the receiver pings every 30 s and drops the phone
//!   after 95 s of silence (measured in HyprMobile Phase 2).
//!
//! Heartbeats go both ways on one frame type: sequences with the top bit set
//! are receiver pings, the rest are phone pings, and each side echoes the
//! other's pings unchanged.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use edge_common::Role;
use edge_crypto::{NoiseSession, PinStatus, PinStore, PinnedPhone};
use edge_protocol::{Frame, HYPR_PHONE_EXTENSION, Heartbeat, Hello, PROTOCOL_VERSION};
use edge_runtime::{SecureFrameReader, SecureFrameSession, SecureFrameWriter};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc,
    time::{self, Instant},
};

use crate::{AbortOnDropTask, append_portable_log, reject_pairing};

/// Unacknowledged data may wait this long before the kernel drops a phone's
/// connection. PC sessions use 5 s; a phone needs a moment to wake and ACK.
#[cfg(target_os = "linux")]
const PHONE_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Set on the sequence of every heartbeat the receiver originates.
const RECEIVER_PING_BIT: u64 = 1 << 63;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PhoneTimings {
    pub ping_interval: Duration,
    pub dead_after: Duration,
}

impl PhoneTimings {
    pub const DEFAULT: Self = Self {
        ping_interval: Duration::from_secs(30),
        dead_after: Duration::from_secs(95),
    };
}

pub(crate) fn is_phone_hello(hello: &Hello) -> bool {
    hello
        .extensions
        .iter()
        .any(|extension| extension == HYPR_PHONE_EXTENSION)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhoneAdmission {
    Accept(PinnedPhone),
    Reject {
        code: &'static str,
        message: &'static str,
    },
}

/// Decides whether an authenticated phone may start a session.
pub(crate) fn admit_phone(pins: &PinStore, name: &str, fingerprint: &str) -> PhoneAdmission {
    if pins.is_pc_key(fingerprint) {
        return PhoneAdmission::Reject {
            code: "key_is_pc",
            message: "this key is paired as a computer, not a phone",
        };
    }
    match pins.phone_status(name, fingerprint) {
        PinStatus::Trusted => PhoneAdmission::Accept(
            pins.phone(name)
                .cloned()
                .expect("trusted phones are pinned"),
        ),
        PinStatus::Unknown => PhoneAdmission::Reject {
            code: "phone_not_paired",
            message: "this phone isn't paired with the computer",
        },
        PinStatus::Changed { .. } => PhoneAdmission::Reject {
            code: "phone_key_changed",
            message: "a different phone is paired under this name; forget it on the computer first",
        },
    }
}

/// What a phone may send. Anything else closes its session.
#[derive(Debug, PartialEq, Eq)]
enum PhoneFrame {
    Heartbeat(u64),
    Error,
    Clipboard,
    NotAllowed(&'static str),
}

fn classify_phone_frame(frame: &Frame) -> PhoneFrame {
    match frame {
        Frame::Heartbeat(Heartbeat { sequence }) => PhoneFrame::Heartbeat(*sequence),
        Frame::Error(_) => PhoneFrame::Error,
        Frame::Clipboard(_) => PhoneFrame::Clipboard,
        Frame::Hello(_) => PhoneFrame::NotAllowed("Hello"),
        Frame::ScreenInfo(_) => PhoneFrame::NotAllowed("ScreenInfo"),
        Frame::Input(_) => PhoneFrame::NotAllowed("Input"),
        Frame::Control(_) => PhoneFrame::NotAllowed("Control"),
        Frame::Audio(_) => PhoneFrame::NotAllowed("Audio"),
        Frame::Pairing(_) => PhoneFrame::NotAllowed("Pairing"),
        Frame::Role(_) => PhoneFrame::NotAllowed("Role"),
    }
}

/// The receiver's half of the phone handshake.
pub(crate) struct LocalHello {
    pub device_name: String,
    pub role: Role,
    pub fingerprint: String,
}

impl LocalHello {
    fn frame(&self) -> Frame {
        Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: self.device_name.clone(),
            role: self.role,
            public_key_fingerprint: self.fingerprint.clone(),
            capabilities: Vec::new(),
            extensions: vec![HYPR_PHONE_EXTENSION.to_string()],
            node_capabilities: Vec::new(),
        })
    }
}

struct RunningPhone {
    name: String,
    task: AbortOnDropTask,
}

/// Live phone sessions, keyed by key fingerprint. Dropping it ends them all.
pub(crate) struct PhoneSessions {
    local: LocalHello,
    log_path: PathBuf,
    timings: PhoneTimings,
    sessions: HashMap<String, RunningPhone>,
}

impl PhoneSessions {
    pub fn new(local: LocalHello, log_path: PathBuf) -> Self {
        Self {
            local,
            log_path,
            timings: PhoneTimings::DEFAULT,
            sessions: HashMap::new(),
        }
    }

    /// Admits an authenticated phone and starts its session. A newer
    /// connection from a connected phone replaces the old session, whose
    /// socket may linger for a while after the phone drops it.
    pub fn start(
        &mut self,
        mut session: NoiseSession<TcpStream>,
        addr: SocketAddr,
        name: String,
        fingerprint: String,
        pins: &PinStore,
    ) {
        self.sessions
            .retain(|_, running| !running.task.0.is_finished());
        let phone = match admit_phone(pins, &name, &fingerprint) {
            PhoneAdmission::Accept(phone) => phone,
            PhoneAdmission::Reject { code, message } => {
                tracing::warn!(%addr, phone = %name, code, "rejected phone");
                append_portable_log(
                    &self.log_path,
                    format!("rejected phone {name} ({addr}): {message}"),
                );
                tokio::spawn(async move {
                    reject_pairing(&mut session, code, message).await;
                });
                return;
            }
        };
        if self.sessions.remove(&fingerprint).is_some() {
            append_portable_log(
                &self.log_path,
                format!("newer connection from phone {name} ({addr}); replacing its session"),
            );
        }
        tracing::info!(%addr, phone = %name, "phone connected");
        append_portable_log(
            &self.log_path,
            format!("phone {name} connected from {addr}"),
        );

        let hello = self.local.frame();
        let log_path = self.log_path.clone();
        let timings = self.timings;
        let task_name = name.clone();
        let task = tokio::spawn(async move {
            let result = run_phone_session(session, hello, phone, timings).await;
            let message = match result {
                Ok(()) => format!("phone {task_name} disconnected"),
                Err(error) => format!("phone {task_name} session ended: {error:#}"),
            };
            tracing::info!(phone = %task_name, "{message}");
            append_portable_log(&log_path, message);
        });
        self.sessions.insert(
            fingerprint,
            RunningPhone {
                name,
                task: AbortOnDropTask(task),
            },
        );
    }

    /// Names of the phones with a live session.
    #[allow(dead_code)] // Used by the tray once it lists phones.
    pub fn connected(&self) -> impl Iterator<Item = &str> {
        self.sessions
            .values()
            .filter(|running| !running.task.0.is_finished())
            .map(|running| running.name.as_str())
    }
}

type PhoneReader = SecureFrameReader<ReadHalf<TcpStream>>;
type PhoneWriter = SecureFrameWriter<WriteHalf<TcpStream>>;

/// Reads frames until the connection fails. Owned by the session, so
/// dropping the session also closes the socket.
fn spawn_phone_reader(mut reader: PhoneReader) -> (mpsc::Receiver<Result<Frame>>, AbortOnDropTask) {
    let (sender, receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        loop {
            let frame = reader.read().await.context("failed to read phone frame");
            let failed = frame.is_err();
            if sender.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    (receiver, AbortOnDropTask(task))
}

async fn run_phone_session(
    session: NoiseSession<TcpStream>,
    hello: Frame,
    _phone: PinnedPhone,
    timings: PhoneTimings,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    socket2::SockRef::from(session.get_ref())
        .set_tcp_user_timeout(Some(PHONE_SOCKET_TIMEOUT))
        .context("failed to set the phone socket timeout")?;

    let (reader, mut writer) = SecureFrameSession::new(session).split();
    writer.write(&hello).await.context("failed to send Hello")?;
    let (mut frames, _reader_task) = spawn_phone_reader(reader);

    let mut ping = time::interval_at(
        Instant::now() + timings.ping_interval,
        timings.ping_interval,
    );
    ping.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut ping_sequence = 0_u64;
    let dead = time::sleep(timings.dead_after);
    tokio::pin!(dead);

    loop {
        tokio::select! {
            _ = ping.tick() => {
                ping_sequence = ping_sequence.wrapping_add(1) & !RECEIVER_PING_BIT;
                send(&mut writer, &Frame::Heartbeat(Heartbeat {
                    sequence: RECEIVER_PING_BIT | ping_sequence,
                }))
                .await?;
            }
            () = &mut dead => {
                anyhow::bail!("phone silent for {:?}", timings.dead_after);
            }
            frame = frames.recv() => {
                let Some(frame) = frame else {
                    anyhow::bail!("phone reader stopped");
                };
                let frame = match frame {
                    Ok(frame) => frame,
                    // A clean close after the phone said goodbye isn't an error.
                    Err(error) if is_closed(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                dead.as_mut().reset(Instant::now() + timings.dead_after);
                match classify_phone_frame(&frame) {
                    PhoneFrame::Heartbeat(sequence) if sequence & RECEIVER_PING_BIT == 0 => {
                        send(&mut writer, &frame).await?;
                    }
                    PhoneFrame::Heartbeat(_) => {}
                    PhoneFrame::Error => tracing::warn!(?frame, "phone reported an error"),
                    // Text clipboard arrives in a later step.
                    PhoneFrame::Clipboard => tracing::debug!("ignored phone clipboard frame"),
                    PhoneFrame::NotAllowed(kind) => {
                        anyhow::bail!("phone sent a {kind} frame, which phones may not send");
                    }
                }
            }
        }
    }
}

async fn send(writer: &mut PhoneWriter, frame: &Frame) -> Result<()> {
    writer
        .write(frame)
        .await
        .context("failed to write to phone")
}

fn is_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use edge_crypto::{IdentityKey, PhoneFeature, accept_noise_session, initiate_noise_session};
    use edge_protocol::{ControlEvent, Edge, InputEvent, RemoteError};
    use tokio::net::TcpListener;

    use super::*;
    use crate::{read_secure_frame, write_secure_frame};

    fn pins_with_phone(name: &str, fingerprint: &str) -> PinStore {
        let mut pins = PinStore::default();
        pins.pin_phone(name, fingerprint, PhoneFeature::all());
        pins
    }

    #[test]
    fn phone_hello_is_recognised_by_its_extension() {
        let mut hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "Pixel".to_string(),
            role: Role::Controller,
            public_key_fingerprint: "fp".to_string(),
            capabilities: Vec::new(),
            extensions: vec!["clipboard-image-v1".to_string()],
            node_capabilities: Vec::new(),
        };
        assert!(!is_phone_hello(&hello));
        hello.extensions.push(HYPR_PHONE_EXTENSION.to_string());
        assert!(is_phone_hello(&hello));
    }

    #[test]
    fn only_pinned_phones_are_admitted() {
        let mut pins = pins_with_phone("Pixel", "phone-key");
        pins.pin("Main PC", "pc-key");

        assert!(matches!(
            admit_phone(&pins, "Pixel", "phone-key"),
            PhoneAdmission::Accept(_)
        ));
        let code = |admission| match admission {
            PhoneAdmission::Reject { code, .. } => code,
            PhoneAdmission::Accept(_) => "accepted",
        };
        assert_eq!(
            code(admit_phone(&pins, "Tablet", "other-key")),
            "phone_not_paired"
        );
        assert_eq!(
            code(admit_phone(&pins, "Pixel", "other-key")),
            "phone_key_changed"
        );
        // A PC's key can't come back as a phone, whatever name it uses.
        assert_eq!(code(admit_phone(&pins, "Pixel", "pc-key")), "key_is_pc");
        assert_eq!(code(admit_phone(&pins, "Main PC", "pc-key")), "key_is_pc");
    }

    #[test]
    fn phones_may_not_send_kvm_frames() {
        let not_allowed = [
            Frame::input(
                1,
                InputEvent::Key {
                    evdev_code: 30,
                    down: true,
                },
            ),
            Frame::control(
                1,
                ControlEvent::EnterRemote {
                    edge: Edge::Left,
                    normalized_position: 0.5,
                },
            ),
            Frame::ScreenInfo(edge_protocol::ScreenInfo {
                outputs: Vec::new(),
                primary_output: String::new(),
            }),
            Frame::Role(edge_protocol::RoleEvent::SetPaused { paused: true }),
            Frame::Pairing(edge_protocol::PairingEvent::Decision { accepted: true }),
        ];
        for frame in not_allowed {
            assert!(
                matches!(classify_phone_frame(&frame), PhoneFrame::NotAllowed(_)),
                "{frame:?}"
            );
        }
        assert_eq!(
            classify_phone_frame(&Frame::Heartbeat(Heartbeat { sequence: 4 })),
            PhoneFrame::Heartbeat(4)
        );
    }

    /// A connected phone session on localhost, driven from the phone's side.
    struct Harness {
        phone: NoiseSession<TcpStream>,
        _session: tokio::task::JoinHandle<Result<()>>,
        ended: tokio::sync::oneshot::Receiver<Result<()>>,
    }

    async fn start_session(timings: PhoneTimings) -> Harness {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver_identity = IdentityKey::generate().unwrap();
        let phone_identity = IdentityKey::generate().unwrap();
        let (ended_tx, ended) = tokio::sync::oneshot::channel();
        let session = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (session, _) = accept_noise_session(stream, &receiver_identity)
                .await
                .unwrap();
            let hello = LocalHello {
                device_name: "laptop".to_string(),
                role: Role::Receiver,
                fingerprint: receiver_identity.fingerprint(),
            }
            .frame();
            let phone = PinnedPhone {
                fingerprint: "phone".to_string(),
                allowed_features: BTreeSet::new(),
            };
            let result = run_phone_session(session, hello, phone, timings).await;
            let summary = match &result {
                Ok(()) => Ok(()),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            };
            ended_tx.send(summary).ok();
            result
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut phone, _) = initiate_noise_session(stream, &phone_identity, None)
            .await
            .unwrap();
        match read_secure_frame(&mut phone).await.unwrap() {
            Frame::Hello(hello) => assert!(is_phone_hello(&hello)),
            other => panic!("expected Hello, got {other:?}"),
        }
        Harness {
            phone,
            _session: session,
            ended,
        }
    }

    const FAST: PhoneTimings = PhoneTimings {
        ping_interval: Duration::from_millis(50),
        dead_after: Duration::from_millis(400),
    };

    async fn ended(harness: Harness) -> Result<()> {
        time::timeout(Duration::from_secs(5), harness.ended)
            .await
            .expect("session should end")
            .unwrap()
    }

    #[tokio::test]
    async fn receiver_pings_and_answers_phone_pings() {
        let mut harness = start_session(FAST).await;

        match read_secure_frame(&mut harness.phone).await.unwrap() {
            Frame::Heartbeat(Heartbeat { sequence }) => {
                assert_ne!(sequence & RECEIVER_PING_BIT, 0, "receiver ping");
            }
            other => panic!("expected a ping, got {other:?}"),
        }
        write_secure_frame(
            &mut harness.phone,
            &Frame::Heartbeat(Heartbeat { sequence: 7 }),
        )
        .await
        .unwrap();
        loop {
            match read_secure_frame(&mut harness.phone).await.unwrap() {
                Frame::Heartbeat(Heartbeat { sequence: 7 }) => break,
                Frame::Heartbeat(Heartbeat { sequence }) if sequence & RECEIVER_PING_BIT != 0 => {}
                other => panic!("expected the echo of 7, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn answering_pings_keeps_a_quiet_phone_connected() {
        let mut harness = start_session(FAST).await;
        let started = Instant::now();
        // Twice the dead time, answering only the receiver's pings.
        while started.elapsed() < FAST.dead_after * 2 {
            let frame = read_secure_frame(&mut harness.phone).await.unwrap();
            write_secure_frame(&mut harness.phone, &frame)
                .await
                .unwrap();
        }
        assert!(
            harness.ended.try_recv().is_err(),
            "session should still be live"
        );
    }

    #[tokio::test]
    async fn silent_phone_is_dropped() {
        let harness = start_session(FAST).await;
        let error = ended(harness).await.unwrap_err();
        assert!(error.to_string().contains("silent"), "{error}");
    }

    #[tokio::test]
    async fn input_frame_closes_the_session() {
        let mut harness = start_session(FAST).await;
        write_secure_frame(
            &mut harness.phone,
            &Frame::input(
                1,
                InputEvent::Key {
                    evdev_code: 30,
                    down: true,
                },
            ),
        )
        .await
        .unwrap();
        let error = ended(harness).await.unwrap_err();
        assert!(error.to_string().contains("Input"), "{error}");
    }

    #[tokio::test]
    async fn phone_errors_are_logged_not_fatal() {
        let mut harness = start_session(FAST).await;
        write_secure_frame(
            &mut harness.phone,
            &Frame::Error(RemoteError {
                code: "test".to_string(),
                message: "phone-side problem".to_string(),
            }),
        )
        .await
        .unwrap();
        write_secure_frame(
            &mut harness.phone,
            &Frame::Heartbeat(Heartbeat { sequence: 1 }),
        )
        .await
        .unwrap();
        loop {
            if let Frame::Heartbeat(Heartbeat { sequence: 1 }) =
                read_secure_frame(&mut harness.phone).await.unwrap()
            {
                break;
            }
        }
    }

    #[tokio::test]
    async fn phone_closing_its_connection_ends_the_session_cleanly() {
        let harness = start_session(FAST).await;
        let Harness { phone, ended, .. } = harness;
        drop(phone);
        let result = time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("session should end")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
    }
}
