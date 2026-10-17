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

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use edge_common::{AppConfig, Role};
use edge_crypto::{NoiseSession, PhoneFeature, PinStatus, PinStore, PinnedPhone};
use edge_protocol::{
    AudioDirection, CLIPBOARD_IMAGE_EXTENSION, ClipboardCancelReason, ClipboardEvent, Frame,
    HYPR_PHONE_EXTENSION, Heartbeat, Hello, NOTIFICATIONS_EXTENSION, PHONE_AUDIO_INVITE_EXTENSION,
    PROTOCOL_VERSION, PhoneEvent, RemoteError,
};
use edge_runtime::{SecureFrameReader, SecureFrameSession, SecureFrameWriter};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::{self, Instant},
};

use crate::{
    AbortOnDropTask, ClipboardWatchEvent, ReceiverClipboardState, append_portable_log,
    clipboard_hub::ClipboardChanges,
    notifications::{FreedesktopNotifications, NotificationMirror},
    phone_audio::{self, PhoneAudio},
    phone_pairing::{PAIRING_WINDOW, PairingRefusal, PairingWindow, lan_addresses, pairing_uri},
    recv_clipboard_change, reject_pairing,
    tray::ReceiverTrayHandle,
};

/// Unacknowledged data may wait this long before the kernel drops a phone's
/// connection. PC sessions use 5 s; a phone needs a moment to wake and ACK.
#[cfg(target_os = "linux")]
const PHONE_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Set on the sequence of every heartbeat the receiver originates.
const RECEIVER_PING_BIT: u64 = 1 << 63;
/// The first ping after connecting, before the regular interval takes over.
const FIRST_PING_AFTER: Duration = Duration::from_secs(2);

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
    /// Not pinned (or re-pairing under a pinned name) while pairing is open:
    /// the phone may present the token from the QR code.
    Pair,
    Reject {
        code: &'static str,
        message: &'static str,
    },
}

/// Decides what an authenticated phone may do.
pub(crate) fn admit_phone(
    pins: &PinStore,
    name: &str,
    fingerprint: &str,
    pairing_open: bool,
) -> PhoneAdmission {
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
        PinStatus::Unknown | PinStatus::Changed { .. } if pairing_open => PhoneAdmission::Pair,
        PinStatus::Unknown => PhoneAdmission::Reject {
            code: "phone_not_paired",
            message: "this phone isn't paired; run `edge-receiver-linux pair-phone` on the computer and scan the code",
        },
        PinStatus::Changed { .. } => PhoneAdmission::Reject {
            code: "phone_key_changed",
            message: "a different phone is paired under this name; pair again with `edge-receiver-linux pair-phone`",
        },
    }
}

/// What a phone may send. Anything else closes its session.
#[derive(Debug, PartialEq, Eq)]
enum PhoneFrame {
    Heartbeat(u64),
    Error,
    Clipboard,
    Notification,
    AudioRequest(Option<AudioDirection>, u16),
    AudioStopped,
    PairRequest(String),
    Unpair,
    NotAllowed(&'static str),
}

fn classify_phone_frame(frame: &Frame) -> PhoneFrame {
    match frame {
        Frame::Heartbeat(Heartbeat { sequence }) => PhoneFrame::Heartbeat(*sequence),
        Frame::Error(_) => PhoneFrame::Error,
        Frame::Clipboard(_) => PhoneFrame::Clipboard,
        Frame::Notification(_) => PhoneFrame::Notification,
        Frame::NotificationSync(_) => PhoneFrame::NotAllowed("NotificationSync"),
        Frame::Phone(PhoneEvent::PairRequest { token }) => PhoneFrame::PairRequest(token.clone()),
        Frame::Phone(PhoneEvent::Unpair) => PhoneFrame::Unpair,
        Frame::Phone(PhoneEvent::PairAccepted) => PhoneFrame::NotAllowed("PairAccepted"),
        Frame::Phone(PhoneEvent::AudioRequest {
            direction,
            udp_port,
        }) => PhoneFrame::AudioRequest(*direction, *udp_port),
        Frame::Phone(PhoneEvent::AudioStopped { .. }) => PhoneFrame::AudioStopped,
        Frame::Phone(PhoneEvent::AudioStart { .. }) => PhoneFrame::NotAllowed("AudioStart"),
        Frame::Phone(PhoneEvent::AudioInvite { .. }) => PhoneFrame::NotAllowed("AudioInvite"),
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
    /// Advertises showing phone notifications and taking clipboard images
    /// when this receiver does.
    fn frame(&self, notifications: bool, images: bool) -> Frame {
        let mut extensions = vec![HYPR_PHONE_EXTENSION.to_string()];
        if notifications {
            extensions.push(NOTIFICATIONS_EXTENSION.to_string());
        }
        if images {
            extensions.push(CLIPBOARD_IMAGE_EXTENSION.to_string());
        }
        Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: self.device_name.clone(),
            role: self.role,
            public_key_fingerprint: self.fingerprint.clone(),
            capabilities: Vec::new(),
            extensions,
            node_capabilities: Vec::new(),
        })
    }
}

/// One phone's notifications on the desktop. The mirror outlives the phone's
/// connections so a reconnect can reconcile instead of re-showing everything.
#[derive(Clone)]
struct SessionNotifications {
    desktop: Arc<FreedesktopNotifications>,
    mirror: Arc<tokio::sync::Mutex<NotificationMirror>>,
}

/// Work for the receiver loop, which owns the pin store, so pin changes
/// happen one at a time.
pub(crate) enum PhoneRequest {
    /// A pairing session presented a token.
    Pair {
        name: String,
        fingerprint: String,
        ip: IpAddr,
        token: String,
        reply: oneshot::Sender<Result<PinnedPhone, PairingRefusal>>,
    },
    /// A paired phone asked to be forgotten.
    Forget { name: String, fingerprint: String },
    /// `pair-phone`: open a pairing window. `paired` receives the phone's
    /// name once one pairs.
    OpenPairing {
        reply: oneshot::Sender<PairingOffer>,
        paired: oneshot::Sender<String>,
    },
    /// `unpair-phone`: replies whether the phone was paired.
    Unpair {
        name: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// `list-phones`: paired phones and whether each is connected.
    List {
        reply: oneshot::Sender<Vec<(String, bool)>>,
    },
    /// From the tray: start a stream this way with a connected phone, or
    /// stop its stream with `None`.
    Audio {
        fingerprint: String,
        direction: Option<AudioDirection>,
    },
}

/// Work for one phone's session.
#[derive(Debug)]
enum PhoneCommand {
    Audio(Option<AudioDirection>),
}

pub(crate) struct PairingOffer {
    pub uri: String,
    pub expires_in: Duration,
}

enum Entry {
    Pinned(PinnedPhone),
    Pairing,
}

/// Everything a session task needs to know about its phone.
struct SessionContext {
    name: String,
    fingerprint: String,
    ip: IpAddr,
    requests: mpsc::UnboundedSender<PhoneRequest>,
    clipboard: Option<ClipboardSource>,
    /// Both sides take clipboard images.
    clipboard_images: bool,
    notifications: Option<SessionNotifications>,
    /// Where audio routing journals live.
    state_dir: PathBuf,
    /// The phone takes `AudioInvite`, so the tray can start streams.
    accepts_audio_invite: bool,
    tray: Option<ReceiverTrayHandle>,
}

/// What a phone session needs to sync the clipboard: the receiver's
/// clipboard settings and a subscription to the shared clipboard hub.
pub(crate) struct ClipboardSource {
    pub config: Arc<AppConfig>,
    pub changes: ClipboardChanges,
}

impl ClipboardSource {
    fn resubscribe(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            changes: self.changes.resubscribe(),
        }
    }
}

/// A phone session's clipboard sync, when the phone is allowed it.
struct PhoneClipboard {
    config: Arc<AppConfig>,
    state: ReceiverClipboardState,
    changes: Option<ClipboardChanges>,
}

impl PhoneClipboard {
    async fn start(source: ClipboardSource, images: bool) -> Result<Self> {
        let state = ReceiverClipboardState::new(&source.config, images)
            .await?
            .for_phone(source.changes.phone_origin());
        Ok(Self {
            config: source.config,
            state,
            changes: Some(source.changes),
        })
    }
}

async fn clipboard_changed(clipboard: &mut Option<PhoneClipboard>) -> ClipboardWatchEvent {
    match clipboard {
        Some(clipboard) => recv_clipboard_change(&mut clipboard.changes).await,
        None => std::future::pending().await,
    }
}

struct RunningPhone {
    name: String,
    pairing: bool,
    commands: mpsc::UnboundedSender<PhoneCommand>,
    task: AbortOnDropTask,
}

/// Live phone sessions, keyed by key fingerprint. Dropping it ends them all.
pub(crate) struct PhoneSessions {
    local: LocalHello,
    listen: SocketAddr,
    log_path: PathBuf,
    clipboard: Option<ClipboardSource>,
    desktop: Option<Arc<FreedesktopNotifications>>,
    /// Per phone key, kept across reconnects.
    mirrors: HashMap<String, Arc<tokio::sync::Mutex<NotificationMirror>>>,
    timings: PhoneTimings,
    sessions: HashMap<String, RunningPhone>,
    pairing: Option<PairingWindow>,
    paired_watcher: Option<oneshot::Sender<String>>,
    requests_tx: mpsc::UnboundedSender<PhoneRequest>,
    requests_rx: mpsc::UnboundedReceiver<PhoneRequest>,
    tray: Option<ReceiverTrayHandle>,
}

impl PhoneSessions {
    pub fn new(
        local: LocalHello,
        listen: SocketAddr,
        log_path: PathBuf,
        clipboard: Option<ClipboardSource>,
    ) -> Self {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        Self {
            local,
            listen,
            log_path,
            clipboard,
            desktop: None,
            mirrors: HashMap::new(),
            timings: PhoneTimings::DEFAULT,
            sessions: HashMap::new(),
            pairing: None,
            paired_watcher: None,
            requests_tx,
            requests_rx,
            tray: None,
        }
    }

    /// Shows connected phones in the tray and takes audio requests from it.
    pub async fn set_tray(&mut self, tray: ReceiverTrayHandle) {
        tray.set_phone_requests(self.requests()).await;
        self.tray = Some(tray);
    }

    /// Enables showing phone notifications on the desktop.
    pub fn set_desktop_notifications(&mut self, desktop: FreedesktopNotifications) {
        self.desktop = Some(Arc::new(desktop));
    }

    /// For the control socket, which sends requests from its own tasks.
    pub fn requests(&self) -> mpsc::UnboundedSender<PhoneRequest> {
        self.requests_tx.clone()
    }

    pub async fn next_request(&mut self) -> PhoneRequest {
        // `self` holds a sender, so the channel never closes.
        self.requests_rx
            .recv()
            .await
            .expect("phone request channel stays open")
    }

    fn pairing_open(&self) -> bool {
        self.pairing
            .as_ref()
            .is_some_and(|window| window.is_open(Instant::now().into_std()))
    }

    /// Admits an authenticated phone and starts its session. A newer
    /// connection from a connected phone replaces the old session, whose
    /// socket may linger for a while after the phone drops it.
    pub fn start(
        &mut self,
        session: NoiseSession<TcpStream>,
        addr: SocketAddr,
        name: String,
        fingerprint: String,
        extensions: &[String],
        pins: &PinStore,
    ) {
        let advertises = |name: &str| extensions.iter().any(|extension| extension == name);
        let phone_takes_images = advertises(CLIPBOARD_IMAGE_EXTENSION);
        self.sessions
            .retain(|_, running| !running.task.0.is_finished());
        let entry = match admit_phone(pins, &name, &fingerprint, self.pairing_open()) {
            PhoneAdmission::Accept(phone) => Entry::Pinned(phone),
            PhoneAdmission::Pair
                if self
                    .sessions
                    .values()
                    .filter(|running| running.pairing)
                    .count()
                    >= MAX_PAIRING_SESSIONS =>
            {
                self.reject(
                    session,
                    addr,
                    &name,
                    "pairing_busy",
                    "too many phones are pairing at once",
                );
                return;
            }
            PhoneAdmission::Pair => Entry::Pairing,
            PhoneAdmission::Reject { code, message } => {
                self.reject(session, addr, &name, code, message);
                return;
            }
        };
        let pairing = matches!(entry, Entry::Pairing);
        if self.sessions.remove(&fingerprint).is_some() {
            append_portable_log(
                &self.log_path,
                format!("newer connection from phone {name} ({addr}); replacing its session"),
            );
        }
        tracing::info!(%addr, phone = %name, pairing, "phone connected");
        append_portable_log(
            &self.log_path,
            if pairing {
                format!("unpaired phone {name} connected from {addr} while pairing is open")
            } else {
                format!("phone {name} connected from {addr}")
            },
        );

        let images = self
            .clipboard
            .as_ref()
            .is_some_and(|source| source.config.clipboard.images_enabled);
        let hello = self.local.frame(self.desktop.is_some(), images);
        let notifications = self.desktop.clone().map(|desktop| SessionNotifications {
            desktop,
            mirror: Arc::clone(self.mirrors.entry(fingerprint.clone()).or_default()),
        });
        let log_path = self.log_path.clone();
        let timings = self.timings;
        let context = SessionContext {
            name: name.clone(),
            fingerprint: fingerprint.clone(),
            ip: addr.ip(),
            requests: self.requests_tx.clone(),
            clipboard: self.clipboard.as_ref().map(ClipboardSource::resubscribe),
            clipboard_images: images && phone_takes_images,
            notifications,
            state_dir: edge_common::default_state_dir(),
            accepts_audio_invite: advertises(PHONE_AUDIO_INVITE_EXTENSION),
            tray: self.tray.clone(),
        };
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let name = context.name.clone();
            let tray = context.tray.clone();
            let fingerprint = context.fingerprint.clone();
            let result =
                run_phone_session(session, hello, entry, context, timings, commands_rx).await;
            if let Some(tray) = tray {
                tray.phone_disconnected(fingerprint).await;
            }
            let message = match result {
                Ok(()) => format!("phone {name} disconnected"),
                Err(error) => format!("phone {name} session ended: {error:#}"),
            };
            tracing::info!(phone = %name, "{message}");
            append_portable_log(&log_path, message);
        });
        self.sessions.insert(
            fingerprint,
            RunningPhone {
                name,
                pairing,
                commands,
                task: AbortOnDropTask(task),
            },
        );
    }

    fn reject(
        &self,
        mut session: NoiseSession<TcpStream>,
        addr: SocketAddr,
        name: &str,
        code: &'static str,
        message: &'static str,
    ) {
        tracing::warn!(%addr, phone = %name, code, "rejected phone");
        append_portable_log(
            &self.log_path,
            format!("rejected phone {name} ({addr}): {message}"),
        );
        tokio::spawn(async move {
            reject_pairing(&mut session, code, message).await;
        });
    }

    /// Handles one request. Pin changes are saved before they're reported.
    pub async fn handle_request(
        &mut self,
        request: PhoneRequest,
        pins: &mut PinStore,
        pins_path: &Path,
    ) {
        match request {
            PhoneRequest::Pair {
                name,
                fingerprint,
                ip,
                token,
                reply,
            } => {
                let result = self
                    .pair(&name, &fingerprint, ip, &token, pins, pins_path)
                    .await;
                if let Err(refusal) = &result {
                    append_portable_log(
                        &self.log_path,
                        format!("refused pairing phone {name} ({ip}): {}", refusal.message()),
                    );
                }
                reply.send(result).ok();
            }
            PhoneRequest::Forget { name, fingerprint } => {
                if pins.phone_status(&name, &fingerprint).is_trusted() {
                    match self.unpair(&name, pins, pins_path).await {
                        Ok(_) => append_portable_log(
                            &self.log_path,
                            format!("phone {name} asked to be forgotten; unpaired"),
                        ),
                        Err(error) => append_portable_log(
                            &self.log_path,
                            format!("failed to forget phone {name}: {error:#}"),
                        ),
                    }
                }
            }
            PhoneRequest::OpenPairing { reply, paired } => {
                let window = PairingWindow::open(Instant::now().into_std());
                let offer = PairingOffer {
                    uri: pairing_uri(
                        &lan_addresses(self.listen),
                        self.listen.port(),
                        &self.local.fingerprint,
                        window.token(),
                    ),
                    expires_in: PAIRING_WINDOW,
                };
                // A new window replaces any earlier one and its token.
                self.pairing = Some(window);
                self.paired_watcher = Some(paired);
                append_portable_log(&self.log_path, "phone pairing opened for 5 minutes");
                reply.send(offer).ok();
            }
            PhoneRequest::Unpair { name, reply } => {
                let result = self
                    .unpair(&name, pins, pins_path)
                    .await
                    .map_err(|error| format!("{error:#}"));
                if let Ok(true) = result {
                    append_portable_log(&self.log_path, format!("unpaired phone {name}"));
                }
                reply.send(result).ok();
            }
            PhoneRequest::List { reply } => {
                let connected: Vec<&str> = self.connected().collect();
                let phones = pins
                    .phones
                    .keys()
                    .map(|name| (name.clone(), connected.contains(&name.as_str())))
                    .collect();
                reply.send(phones).ok();
            }
            PhoneRequest::Audio {
                fingerprint,
                direction,
            } => {
                if let Some(running) = self.sessions.get(&fingerprint) {
                    running.commands.send(PhoneCommand::Audio(direction)).ok();
                }
            }
        }
    }

    async fn pair(
        &mut self,
        name: &str,
        fingerprint: &str,
        ip: IpAddr,
        token: &str,
        pins: &mut PinStore,
        pins_path: &Path,
    ) -> Result<PinnedPhone, PairingRefusal> {
        let window = self.pairing.as_mut().ok_or(PairingRefusal::Closed)?;
        window.check(token, ip, Instant::now().into_std())?;
        if pins.is_pc_key(fingerprint) {
            return Err(PairingRefusal::Failed);
        }
        let previous = pins.phones.clone();
        // One pin per phone key: drop any older entry under another name.
        pins.phones
            .retain(|_, pinned| pinned.fingerprint != fingerprint);
        pins.pin_phone(name, fingerprint, PhoneFeature::all());
        if let Err(error) = pins.save(pins_path).await {
            pins.phones = previous;
            append_portable_log(
                &self.log_path,
                format!("failed to save phone pairing: {error:#}"),
            );
            return Err(PairingRefusal::Failed);
        }
        // Saved: only now is the token used up.
        self.pairing = None;
        if let Some(watcher) = self.paired_watcher.take() {
            watcher.send(name.to_string()).ok();
        }
        tracing::info!(phone = %name, %fingerprint, "paired phone");
        append_portable_log(
            &self.log_path,
            format!("paired phone {name} ({fingerprint})"),
        );
        Ok(pins.phone(name).cloned().expect("just pinned"))
    }

    /// Removes the pin, saves, then closes the phone's session. Returns
    /// whether the phone was paired.
    async fn unpair(&mut self, name: &str, pins: &mut PinStore, pins_path: &Path) -> Result<bool> {
        let Some(removed) = pins.remove_phone(name) else {
            return Ok(false);
        };
        if let Err(error) = pins.save(pins_path).await {
            pins.phones.insert(name.to_string(), removed);
            return Err(error).context("failed to save pins");
        }
        // Dropping the session aborts it and closes its socket.
        self.sessions.remove(&removed.fingerprint);
        if let Some(tray) = &self.tray {
            tray.phone_disconnected(removed.fingerprint.clone()).await;
        }
        let cache = edge_common::default_state_dir()
            .join("phone-notifications")
            .join(format!("{}.json", removed.fingerprint.replace(':', "")));
        if let Some(desktop) = &self.desktop {
            let mirror = self
                .mirrors
                .remove(&removed.fingerprint)
                .unwrap_or_default();
            let mut mirror = mirror.lock().await;
            if let Err(error) = mirror.restore(desktop.as_ref(), &cache).await {
                tracing::warn!(%error, "couldn't restore phone notification identities for removal");
            }
            mirror.close_all(desktop.as_ref()).await;
        }
        match tokio::fs::remove_file(cache).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, "couldn't remove unpaired phone notification cache")
            }
        }
        Ok(true)
    }

    /// Names of the phones with a live session.
    pub fn connected(&self) -> impl Iterator<Item = &str> {
        self.sessions
            .values()
            .filter(|running| !running.pairing && !running.task.0.is_finished())
            .map(|running| running.name.as_str())
    }
}

/// Unpinned phones allowed to be mid-pairing at once.
const MAX_PAIRING_SESSIONS: usize = 4;
/// An unpinned phone must present its token this quickly.
const PAIR_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Waits for an unpinned phone's token and asks the receiver loop to pin it.
async fn pair_phone(
    frames: &mut mpsc::Receiver<Result<Frame>>,
    writer: &mut PhoneWriter,
    context: &SessionContext,
) -> Result<PinnedPhone> {
    let frame = match time::timeout(PAIR_REQUEST_TIMEOUT, frames.recv()).await {
        Ok(Some(frame)) => frame?,
        Ok(None) => anyhow::bail!("phone reader stopped"),
        Err(_) => anyhow::bail!("unpaired phone sent no pairing request"),
    };
    let PhoneFrame::PairRequest(token) = classify_phone_frame(&frame) else {
        anyhow::bail!("unpaired phone sent {frame:?} instead of a pairing request");
    };
    let (reply, outcome) = oneshot::channel();
    context
        .requests
        .send(PhoneRequest::Pair {
            name: context.name.clone(),
            fingerprint: context.fingerprint.clone(),
            ip: context.ip,
            token,
            reply,
        })
        .context("receiver loop stopped")?;
    match outcome.await.context("receiver loop stopped")? {
        Ok(phone) => {
            send(writer, &Frame::Phone(PhoneEvent::PairAccepted)).await?;
            Ok(phone)
        }
        Err(refusal) => {
            send(
                writer,
                &Frame::Error(RemoteError {
                    code: refusal.code().to_string(),
                    message: refusal.message().to_string(),
                }),
            )
            .await
            .ok();
            anyhow::bail!("pairing refused: {}", refusal.message());
        }
    }
}

async fn run_phone_session(
    session: NoiseSession<TcpStream>,
    hello: Frame,
    entry: Entry,
    context: SessionContext,
    timings: PhoneTimings,
    mut commands: mpsc::UnboundedReceiver<PhoneCommand>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    socket2::SockRef::from(session.get_ref())
        .set_tcp_user_timeout(Some(PHONE_SOCKET_TIMEOUT))
        .context("failed to set the phone socket timeout")?;

    let (reader, mut writer) = SecureFrameSession::new(session).split();
    writer.write(&hello).await.context("failed to send Hello")?;
    let (mut frames, _reader_task) = spawn_phone_reader(reader);
    let phone = match entry {
        Entry::Pinned(phone) => phone,
        Entry::Pairing => pair_phone(&mut frames, &mut writer, &context).await?,
    };
    if let Some(tray) = &context.tray {
        tray.phone_connected(
            context.fingerprint.clone(),
            context.name.clone(),
            context.accepts_audio_invite,
        )
        .await;
    }
    let clipboard_allowed = phone.allowed_features.contains(&PhoneFeature::Clipboard);
    let mut image_pace = time::interval(Duration::from_millis(2));
    image_pace.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut image_expiry = time::interval(Duration::from_secs(1));
    let audio_allowed = phone.allowed_features.contains(&PhoneFeature::Audio);
    let (audio_stopped_tx, mut audio_stopped) = mpsc::unbounded_channel::<(u64, String)>();
    let mut audio: Option<PhoneAudio> = None;
    let mut audio_generation = 0_u64;
    let notifications = context.notifications.clone().filter(|_| {
        phone
            .allowed_features
            .contains(&PhoneFeature::Notifications)
    });
    if let Some(notifications) = &notifications {
        let path = context
            .state_dir
            .join("phone-notifications")
            .join(format!("{}.json", context.fingerprint.replace(':', "")));
        if let Err(error) = notifications
            .mirror
            .lock()
            .await
            .restore(notifications.desktop.as_ref(), &path)
            .await
        {
            tracing::warn!(%error, phone = %context.name, "couldn't restore phone notification identities");
        }
    }
    let mut clipboard = match context.clipboard {
        Some(source) if clipboard_allowed => {
            match PhoneClipboard::start(source, context.clipboard_images).await {
                Ok(clipboard) => Some(clipboard),
                Err(error) => {
                    tracing::warn!(%error, phone = %context.name, "phone clipboard sync unavailable");
                    None
                }
            }
        }
        _ => None,
    };

    // The first ping goes out soon, so the tray shows a round trip early.
    let mut ping = time::interval_at(
        Instant::now() + timings.ping_interval.min(FIRST_PING_AFTER),
        timings.ping_interval,
    );
    ping.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut ping_sequence = 0_u64;
    let mut ping_sent: Option<(u64, Instant)> = None;
    let mut reported_audio: Option<AudioDirection> = None;
    let dead = time::sleep(timings.dead_after);
    tokio::pin!(dead);

    loop {
        let audio_now = audio.as_ref().map(|running| running.direction);
        if audio_now != reported_audio {
            reported_audio = audio_now;
            if let Some(tray) = &context.tray {
                tray.phone_audio(context.fingerprint.clone(), audio_now)
                    .await;
            }
        }
        tokio::select! {
            _ = ping.tick() => {
                ping_sequence = ping_sequence.wrapping_add(1) & !RECEIVER_PING_BIT;
                let sequence = RECEIVER_PING_BIT | ping_sequence;
                send(&mut writer, &Frame::Heartbeat(Heartbeat { sequence })).await?;
                ping_sent = Some((sequence, Instant::now()));
            }
            Some(command) = commands.recv() => match command {
                PhoneCommand::Audio(Some(direction)) if context.accepts_audio_invite => {
                    send(&mut writer, &Frame::Phone(PhoneEvent::AudioInvite { direction })).await?;
                }
                PhoneCommand::Audio(Some(_)) => {
                    tracing::debug!(phone = %context.name, "phone can't be asked to start audio");
                }
                PhoneCommand::Audio(None) => {
                    if audio.take().is_some() {
                        send(&mut writer, &Frame::Phone(PhoneEvent::AudioStopped {
                            reason: "stopped on the computer".into(),
                        }))
                        .await?;
                    }
                }
            },
            Some((generation, reason)) = audio_stopped.recv() => {
                if audio.as_ref().is_some_and(|running| running.generation == generation) {
                    audio = None;
                    tracing::info!(phone = %context.name, %reason, "phone audio ended");
                    send(&mut writer, &Frame::Phone(PhoneEvent::AudioStopped { reason })).await?;
                }
            }
            () = &mut dead => {
                anyhow::bail!("phone silent for {:?}", timings.dead_after);
            }
            // Image chunks go out between other frames.
            _ = image_pace.tick(), if clipboard.as_ref().is_some_and(|sync| sync.state.outgoing.is_some()) => {
                let Some(sync) = clipboard.as_mut() else { continue };
                if let Err(error) = sync.state.send_next_image_frame(&mut writer).await {
                    tracing::warn!(%error, phone = %context.name, "couldn't send clipboard image to phone");
                    sync.state.outgoing = None;
                }
            }
            _ = image_expiry.tick(), if clipboard.is_some() => {
                let Some(sync) = clipboard.as_mut() else { continue };
                if let Some(transfer_id) = sync.state.incoming.expire_transfer_id() {
                    send(&mut writer, &Frame::Clipboard(ClipboardEvent::ImageCancel {
                        transfer_id,
                        reason: ClipboardCancelReason::TimedOut,
                    }))
                    .await?;
                }
            }
            event = clipboard_changed(&mut clipboard) => {
                let Some(sync) = clipboard.as_mut() else { continue };
                match event {
                    ClipboardWatchEvent::Changed => {
                        match sync.state.send_changed_offer(&sync.config, &mut writer).await {
                            Ok(true) => tracing::info!(phone = %context.name, "sent clipboard to phone"),
                            Ok(false) => {}
                            // Too large for one frame, for example; the session carries on.
                            Err(error) => tracing::warn!(%error, phone = %context.name, "couldn't send clipboard to phone"),
                        }
                    }
                    ClipboardWatchEvent::Closed => sync.changes = None,
                }
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
                    PhoneFrame::Heartbeat(sequence) => {
                        if let Some((sent_sequence, sent)) = ping_sent
                            && sent_sequence == sequence
                        {
                            ping_sent = None;
                            if let Some(tray) = &context.tray {
                                tray.phone_round_trip(context.fingerprint.clone(), sent.elapsed())
                                    .await;
                            }
                        }
                    }
                    PhoneFrame::Error => tracing::warn!(?frame, "phone reported an error"),
                    PhoneFrame::Notification => {
                        let (Some(notifications), Frame::Notification(event)) = (&notifications, frame) else {
                            tracing::debug!(phone = %context.name, "ignored notification from a phone without notification access");
                            continue;
                        };
                        let mut mirror = notifications.mirror.lock().await;
                        if let Err(error) = mirror.apply(event, notifications.desktop.as_ref(), &context.name).await {
                            tracing::warn!(%error, phone = %context.name, "couldn't show phone notification");
                        }
                    }
                    PhoneFrame::Clipboard => {
                        let (Some(sync), Frame::Clipboard(event)) = (clipboard.as_mut(), frame) else {
                            tracing::debug!(phone = %context.name, "ignored clipboard from a phone without clipboard access");
                            continue;
                        };
                        match sync.state.handle_event(&sync.config, &mut writer, event).await {
                            Ok(true) => tracing::info!(phone = %context.name, "updated clipboard from phone"),
                            Ok(false) => {}
                            Err(error) => tracing::warn!(%error, phone = %context.name, "couldn't apply clipboard from phone"),
                        }
                    }
                    // Already pinned: the phone missed our earlier reply.
                    PhoneFrame::PairRequest(_) => {
                        send(&mut writer, &Frame::Phone(PhoneEvent::PairAccepted)).await?;
                    }
                    PhoneFrame::AudioRequest(direction, udp_port) => {
                        // One direction at a time: whatever ran stops first.
                        audio = None;
                        let Some(direction) = direction else { continue };
                        if !audio_allowed {
                            send(&mut writer, &Frame::Phone(PhoneEvent::AudioStopped {
                                reason: "audio isn't allowed for this phone".into(),
                            }))
                            .await?;
                            continue;
                        }
                        audio_generation += 1;
                        let (start, running) = phone_audio::start(
                            direction,
                            SocketAddr::new(context.ip, udp_port),
                            context.state_dir.clone(),
                            audio_generation,
                            audio_stopped_tx.clone(),
                        );
                        send(&mut writer, &start).await?;
                        tracing::info!(phone = %context.name, ?direction, "phone audio starting");
                        audio = Some(running);
                    }
                    PhoneFrame::AudioStopped => audio = None,
                    PhoneFrame::Unpair => {
                        context
                            .requests
                            .send(PhoneRequest::Forget {
                                name: context.name.clone(),
                                fingerprint: context.fingerprint.clone(),
                            })
                            .ok();
                        return Ok(());
                    }
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
    error
        .chain()
        .filter_map(io_error)
        .any(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
}

/// The io error behind `cause`. The crates' io variants are transparent, so
/// it's the variant's payload rather than a separate cause in the chain.
fn io_error<'a>(cause: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    use edge_crypto::CryptoError;
    use edge_protocol::ProtocolError;
    use edge_runtime::RuntimeError;
    match (
        cause.downcast_ref::<std::io::Error>(),
        cause.downcast_ref::<RuntimeError>(),
        cause.downcast_ref::<CryptoError>(),
        cause.downcast_ref::<ProtocolError>(),
    ) {
        (Some(io), ..)
        | (_, Some(RuntimeError::Crypto(CryptoError::Io(io))), ..)
        | (_, Some(RuntimeError::Protocol(ProtocolError::Io(io))), ..)
        | (_, _, Some(CryptoError::Io(io)), _)
        | (_, _, _, Some(ProtocolError::Io(io))) => Some(io),
        _ => None,
    }
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

        let code = |admission| match admission {
            PhoneAdmission::Reject { code, .. } => code,
            PhoneAdmission::Accept(_) => "accepted",
            PhoneAdmission::Pair => "pair",
        };
        for pairing_open in [false, true] {
            assert_eq!(
                code(admit_phone(&pins, "Pixel", "phone-key", pairing_open)),
                "accepted"
            );
            // A PC's key can't come back as a phone, whatever name it uses.
            assert_eq!(
                code(admit_phone(&pins, "Pixel", "pc-key", pairing_open)),
                "key_is_pc"
            );
            assert_eq!(
                code(admit_phone(&pins, "Main PC", "pc-key", pairing_open)),
                "key_is_pc"
            );
        }
        assert_eq!(
            code(admit_phone(&pins, "Tablet", "other-key", false)),
            "phone_not_paired"
        );
        assert_eq!(
            code(admit_phone(&pins, "Pixel", "other-key", false)),
            "phone_key_changed"
        );
        // While pairing is open, the token decides.
        assert_eq!(
            code(admit_phone(&pins, "Tablet", "other-key", true)),
            "pair"
        );
        assert_eq!(code(admit_phone(&pins, "Pixel", "other-key", true)), "pair");
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
        requests: mpsc::UnboundedReceiver<PhoneRequest>,
        _session: tokio::task::JoinHandle<Result<()>>,
        ended: tokio::sync::oneshot::Receiver<Result<()>>,
    }

    fn pinned() -> Entry {
        Entry::Pinned(PinnedPhone {
            fingerprint: "phone".to_string(),
            allowed_features: BTreeSet::new(),
        })
    }

    async fn start_session(timings: PhoneTimings) -> Harness {
        start_session_as(timings, pinned()).await
    }

    async fn start_session_as(timings: PhoneTimings, entry: Entry) -> Harness {
        let (requests_tx, requests) = mpsc::unbounded_channel();
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
            .frame(false, false);
            let context = SessionContext {
                name: "Pixel".to_string(),
                fingerprint: "phone".to_string(),
                ip: "127.0.0.1".parse().unwrap(),
                requests: requests_tx,
                clipboard: None,
                clipboard_images: false,
                notifications: None,
                state_dir: std::env::temp_dir(),
                accepts_audio_invite: false,
                tray: None,
            };
            let (_commands, commands) = mpsc::unbounded_channel();
            let result = run_phone_session(session, hello, entry, context, timings, commands).await;
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
            requests,
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

    async fn next_frame(phone: &mut NoiseSession<TcpStream>) -> Frame {
        loop {
            match time::timeout(Duration::from_secs(5), read_secure_frame(phone))
                .await
                .expect("frame should arrive")
                .unwrap()
            {
                // Skip the receiver's pings.
                Frame::Heartbeat(Heartbeat { sequence }) if sequence & RECEIVER_PING_BIT != 0 => {}
                frame => return frame,
            }
        }
    }

    async fn send_pair_request(phone: &mut NoiseSession<TcpStream>, token: &str) {
        write_secure_frame(
            phone,
            &Frame::Phone(PhoneEvent::PairRequest {
                token: token.to_string(),
            }),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pairing_session_pins_then_carries_on_as_a_normal_session() {
        let mut harness = start_session_as(FAST, Entry::Pairing).await;
        send_pair_request(&mut harness.phone, "token").await;

        let Some(PhoneRequest::Pair {
            name, token, reply, ..
        }) = harness.requests.recv().await
        else {
            panic!("expected a pair request");
        };
        assert_eq!((name.as_str(), token.as_str()), ("Pixel", "token"));
        reply
            .send(Ok(PinnedPhone {
                fingerprint: "phone".to_string(),
                allowed_features: PhoneFeature::all(),
            }))
            .ok();
        assert_eq!(
            next_frame(&mut harness.phone).await,
            Frame::Phone(PhoneEvent::PairAccepted)
        );

        write_secure_frame(
            &mut harness.phone,
            &Frame::Heartbeat(Heartbeat { sequence: 3 }),
        )
        .await
        .unwrap();
        assert_eq!(
            next_frame(&mut harness.phone).await,
            Frame::Heartbeat(Heartbeat { sequence: 3 })
        );
    }

    #[tokio::test]
    async fn refused_pairing_reports_why_and_closes() {
        let mut harness = start_session_as(FAST, Entry::Pairing).await;
        send_pair_request(&mut harness.phone, "wrong").await;
        let Some(PhoneRequest::Pair { reply, .. }) = harness.requests.recv().await else {
            panic!("expected a pair request");
        };
        reply.send(Err(PairingRefusal::WrongToken)).ok();

        match next_frame(&mut harness.phone).await {
            Frame::Error(RemoteError { code, .. }) => assert_eq!(code, "pairing_wrong_token"),
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(ended(harness).await.is_err());
    }

    #[tokio::test]
    async fn unpaired_phone_must_start_with_a_pair_request() {
        let mut harness = start_session_as(FAST, Entry::Pairing).await;
        write_secure_frame(
            &mut harness.phone,
            &Frame::Heartbeat(Heartbeat { sequence: 1 }),
        )
        .await
        .unwrap();
        let error = ended(harness).await.unwrap_err();
        assert!(
            error.to_string().contains("instead of a pairing request"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn pinned_phone_repeating_its_request_is_accepted_again() {
        let mut harness = start_session(FAST).await;
        send_pair_request(&mut harness.phone, "stale-token").await;
        assert_eq!(
            next_frame(&mut harness.phone).await,
            Frame::Phone(PhoneEvent::PairAccepted)
        );
        assert!(harness.requests.try_recv().is_err(), "no pin change needed");
    }

    #[tokio::test]
    async fn unpair_from_the_phone_asks_to_forget_it() {
        let mut harness = start_session(FAST).await;
        write_secure_frame(&mut harness.phone, &Frame::Phone(PhoneEvent::Unpair))
            .await
            .unwrap();
        match harness.requests.recv().await {
            Some(PhoneRequest::Forget { name, fingerprint }) => {
                assert_eq!((name.as_str(), fingerprint.as_str()), ("Pixel", "phone"));
            }
            _ => panic!("expected a forget request"),
        }
        assert!(ended(harness).await.is_ok());
    }

    fn sessions() -> PhoneSessions {
        PhoneSessions::new(
            LocalHello {
                device_name: "laptop".to_string(),
                role: Role::Receiver,
                fingerprint: "receiver-fp".to_string(),
            },
            "192.168.0.11:42420".parse().unwrap(),
            std::env::temp_dir().join("edge-kvm-phone-test.log"),
            None,
        )
    }

    async fn open_pairing(
        phones: &mut PhoneSessions,
        pins: &mut PinStore,
        path: &Path,
    ) -> (String, oneshot::Receiver<String>) {
        let (reply, offer) = oneshot::channel();
        let (paired_tx, paired) = oneshot::channel();
        phones
            .handle_request(
                PhoneRequest::OpenPairing {
                    reply,
                    paired: paired_tx,
                },
                pins,
                path,
            )
            .await;
        let uri = offer.await.unwrap().uri;
        let token = uri
            .split('&')
            .find_map(|part| part.strip_prefix("token="))
            .unwrap()
            .to_string();
        assert!(
            uri.contains("fp=receiver-fp") && uri.contains("port=42420"),
            "{uri}"
        );
        (token, paired)
    }

    async fn request_pair(
        phones: &mut PhoneSessions,
        pins: &mut PinStore,
        path: &Path,
        name: &str,
        fingerprint: &str,
        token: &str,
    ) -> Result<PinnedPhone, PairingRefusal> {
        let (reply, outcome) = oneshot::channel();
        phones
            .handle_request(
                PhoneRequest::Pair {
                    name: name.to_string(),
                    fingerprint: fingerprint.to_string(),
                    ip: "192.168.0.21".parse().unwrap(),
                    token: token.to_string(),
                    reply,
                },
                pins,
                path,
            )
            .await;
        outcome.await.unwrap()
    }

    #[tokio::test]
    async fn the_token_pins_exactly_one_phone_and_is_then_used_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut pins = PinStore::default();
        pins.pin("Main PC", "pc-key");
        let mut phones = sessions();
        let (token, paired) = open_pairing(&mut phones, &mut pins, &path).await;

        assert_eq!(
            request_pair(&mut phones, &mut pins, &path, "Pixel", "key-a", "wrong").await,
            Err(PairingRefusal::WrongToken)
        );
        assert!(
            request_pair(&mut phones, &mut pins, &path, "Pixel", "key-a", &token)
                .await
                .is_ok()
        );
        assert_eq!(paired.await.unwrap(), "Pixel");
        // A second phone racing with the same token loses.
        assert_eq!(
            request_pair(&mut phones, &mut pins, &path, "Tablet", "key-b", &token).await,
            Err(PairingRefusal::Closed)
        );

        let saved = PinStore::load_or_default(&path).await.unwrap();
        assert!(saved.phone_status("Pixel", "key-a").is_trusted());
        assert!(saved.phone("Tablet").is_none());
        assert!(saved.status("Main PC", "pc-key").is_trusted());
    }

    #[tokio::test]
    async fn a_pc_key_cannot_pair_as_a_phone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut pins = PinStore::default();
        pins.pin("Main PC", "pc-key");
        let mut phones = sessions();
        let (token, _paired) = open_pairing(&mut phones, &mut pins, &path).await;

        assert_eq!(
            request_pair(&mut phones, &mut pins, &path, "Pixel", "pc-key", &token).await,
            Err(PairingRefusal::Failed)
        );
        assert!(pins.phones.is_empty());
    }

    #[tokio::test]
    async fn a_failed_save_keeps_the_token_usable() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the pin file should be makes the save fail.
        let blocked = dir.path().join("pins.toml");
        std::fs::create_dir(&blocked).unwrap();
        let mut pins = PinStore::default();
        let mut phones = sessions();
        let (token, _paired) = open_pairing(&mut phones, &mut pins, &blocked).await;

        assert_eq!(
            request_pair(&mut phones, &mut pins, &blocked, "Pixel", "key-a", &token).await,
            Err(PairingRefusal::Failed)
        );
        assert!(pins.phones.is_empty(), "the in-memory pin is rolled back");

        let path = dir.path().join("pins-ok.toml");
        assert!(
            request_pair(&mut phones, &mut pins, &path, "Pixel", "key-a", &token)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn re_pairing_a_phone_replaces_its_old_key_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut pins = pins_with_phone("Old name", "key-a");
        pins.pin_phone("Pixel", "old-key", PhoneFeature::all());
        let mut phones = sessions();
        let (token, _paired) = open_pairing(&mut phones, &mut pins, &path).await;

        assert!(
            request_pair(&mut phones, &mut pins, &path, "Pixel", "key-a", &token)
                .await
                .is_ok()
        );
        assert!(pins.phone_status("Pixel", "key-a").is_trusted());
        assert!(pins.phone("Old name").is_none(), "one pin per phone key");
    }

    #[tokio::test]
    async fn unpair_removes_and_saves_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut pins = pins_with_phone("Pixel", "key-a");
        let mut phones = sessions();

        for expected in [Ok(true), Ok(false)] {
            let (reply, result) = oneshot::channel();
            phones
                .handle_request(
                    PhoneRequest::Unpair {
                        name: "Pixel".to_string(),
                        reply,
                    },
                    &mut pins,
                    &path,
                )
                .await;
            assert_eq!(result.await.unwrap(), expected);
        }
        assert!(
            PinStore::load_or_default(&path)
                .await
                .unwrap()
                .phones
                .is_empty()
        );
    }

    #[tokio::test]
    async fn audio_is_refused_when_the_pin_does_not_allow_it() {
        // The test harness pins the phone with no features.
        let mut harness = start_session(FAST).await;
        write_secure_frame(
            &mut harness.phone,
            &Frame::Phone(PhoneEvent::AudioRequest {
                direction: Some(AudioDirection::ToPhone),
                udp_port: 9,
            }),
        )
        .await
        .unwrap();
        match next_frame(&mut harness.phone).await {
            Frame::Phone(PhoneEvent::AudioStopped { reason }) => {
                assert!(reason.contains("isn't allowed"), "{reason}");
            }
            other => panic!("expected AudioStopped, got {other:?}"),
        }
    }
}
