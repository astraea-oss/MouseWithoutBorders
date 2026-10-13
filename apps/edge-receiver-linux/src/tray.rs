use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use edge_protocol::AudioDirection;
use ksni::TrayMethods;
use tokio::{
    sync::{mpsc, watch},
    time,
};

use crate::phone::PhoneRequest;

/// Counters and the round trip reach the tray at most this often, so an open
/// menu isn't relaid out on every heartbeat.
const LIVE_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const TRAY_UPDATE_TIMEOUT: Duration = Duration::from_millis(500);
/// Stored in place of a round trip that hasn't been measured.
const NO_ROUND_TRIP: u64 = u64::MAX;

#[derive(Debug)]
pub enum TrayCommand {
    OpenSettings,
    AddPhone,
    ArmPairing,
    Disconnect,
    Reconnect,
    ToggleInputForwarding,
    SetAudio(AudioChoice),
    SetController(ControllerChoice),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerChoice {
    Local,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChoice {
    Off,
    Local,
    Peer,
}

/// Where audio goes, as picked from the tray. One destination at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioTarget {
    /// To or from the PC peer.
    Pc(AudioChoice),
    /// This computer's audio to the phone with this key.
    ToPhone(String),
    /// The phone's audio here. Only the phone can start this.
    FromPhone(String),
    Off,
}

/// A connected phone, as the tray shows it.
#[derive(Debug, Clone)]
struct TrayPhone {
    fingerprint: String,
    name: String,
    round_trip: Option<Duration>,
    audio: Option<AudioDirection>,
    /// The phone takes `AudioInvite`, so audio can be sent to it from here.
    accepts_audio_invite: bool,
}

/// What the tray shows, published on every change for the settings window,
/// which reads it over the control socket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayStatus {
    pub summary: String,
    pub connected: bool,
    pub peer: Option<String>,
    pub round_trip_ms: Option<u64>,
    pub diagnostics: Vec<(&'static str, String)>,
}

/// Updated from session loops without waiting on the tray service.
struct LiveCounters {
    input_events: AtomicU64,
    clipboard_events: AtomicU64,
    round_trip_micros: AtomicU64,
    dirty: AtomicBool,
}

impl LiveCounters {
    fn new() -> Self {
        Self {
            input_events: AtomicU64::new(0),
            clipboard_events: AtomicU64::new(0),
            round_trip_micros: AtomicU64::new(NO_ROUND_TRIP),
            dirty: AtomicBool::new(false),
        }
    }

    fn round_trip(&self) -> Option<Duration> {
        match self.round_trip_micros.load(Ordering::Relaxed) {
            NO_ROUND_TRIP => None,
            micros => Some(Duration::from_micros(micros)),
        }
    }
}

#[derive(Clone)]
pub struct ReceiverTrayHandle {
    handle: ksni::Handle<ReceiverTray>,
    live: Arc<LiveCounters>,
    status: Arc<watch::Sender<TrayStatus>>,
    commands: mpsc::UnboundedSender<TrayCommand>,
}

impl ReceiverTrayHandle {
    pub async fn spawn(
        transport: String,
        backend: String,
        pairing_armed: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TrayCommand>), ksni::Error> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let tray = ReceiverTray::new(transport, backend, pairing_armed, command_tx.clone());
        let status = Arc::new(watch::Sender::new(tray.status()));
        let handle = tray.assume_sni_available(true).spawn().await?;
        let live = Arc::new(LiveCounters::new());
        spawn_live_updates(handle.clone(), live.clone(), status.clone());
        Ok((
            Self {
                handle,
                live,
                status,
                commands: command_tx,
            },
            command_rx,
        ))
    }

    /// The tray's status, refreshed on every change.
    pub fn status(&self) -> watch::Receiver<TrayStatus> {
        self.status.subscribe()
    }

    /// Queues a command as if it came from the menu.
    pub fn command(&self, command: TrayCommand) {
        let _ = self.commands.send(command);
    }

    /// Lets the menu pair phones and route audio to them.
    pub async fn set_phone_requests(&self, requests: mpsc::UnboundedSender<PhoneRequest>) {
        self.update(move |tray| tray.phone_requests = Some(requests))
            .await;
    }

    pub async fn listening(&self) {
        self.set_round_trip(None);
        self.update(|tray| {
            tray.state = TrayState::Listening;
            tray.connected_peer = None;
            tray.round_trip = None;
            tray.last_error = None;
        })
        .await;
    }

    #[cfg(target_os = "linux")]
    pub async fn connecting(&self) {
        self.set_round_trip(None);
        self.update(|tray| {
            tray.state = TrayState::Starting;
            tray.connected_peer = None;
            tray.round_trip = None;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn pairing_armed(&self, armed: bool) {
        self.update(move |tray| tray.pairing_armed = armed).await;
    }

    pub async fn connected(&self, peer: String) {
        self.set_round_trip(None);
        let input_events = self.live.input_events.load(Ordering::Relaxed);
        let clipboard_events = self.live.clipboard_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = TrayState::Connected;
            tray.connected_peer = Some(peer);
            tray.round_trip = None;
            tray.connections = tray.connections.saturating_add(1);
            tray.input_events = input_events;
            tray.clipboard_events = clipboard_events;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn disconnected(&self, error: Option<String>) {
        self.set_round_trip(None);
        let input_events = self.live.input_events.load(Ordering::Relaxed);
        let clipboard_events = self.live.clipboard_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = if error.is_some() {
                TrayState::Error
            } else {
                TrayState::Listening
            };
            tray.connected_peer = None;
            tray.round_trip = None;
            tray.input_events = input_events;
            tray.clipboard_events = clipboard_events;
            tray.last_error = error;
        })
        .await;
    }

    pub async fn disconnected_by_user(&self) {
        self.set_round_trip(None);
        let input_events = self.live.input_events.load(Ordering::Relaxed);
        let clipboard_events = self.live.clipboard_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = TrayState::Paused;
            tray.connected_peer = None;
            tray.round_trip = None;
            tray.input_events = input_events;
            tray.clipboard_events = clipboard_events;
            tray.last_error = None;
        })
        .await;
    }

    pub fn input_event(&self) {
        self.live.input_events.fetch_add(1, Ordering::Relaxed);
        self.live.dirty.store(true, Ordering::Release);
    }

    pub fn clipboard_event(&self) {
        self.live.clipboard_events.fetch_add(1, Ordering::Relaxed);
        self.live.dirty.store(true, Ordering::Release);
    }

    /// The latest heartbeat round trip to the PC peer.
    pub fn peer_round_trip(&self, round_trip: Option<Duration>) {
        if self.live.round_trip() != round_trip {
            self.set_round_trip(round_trip);
            self.live.dirty.store(true, Ordering::Release);
        }
    }

    fn set_round_trip(&self, round_trip: Option<Duration>) {
        let micros = round_trip.map_or(NO_ROUND_TRIP, |rtt| {
            u64::try_from(rtt.as_micros()).unwrap_or(NO_ROUND_TRIP - 1)
        });
        self.live.round_trip_micros.store(micros, Ordering::Relaxed);
    }

    pub async fn input_forwarding(&self, enabled: bool) {
        self.update(move |tray| tray.input_forwarding_enabled = enabled)
            .await;
    }

    pub async fn session_paused(&self, paused: bool) {
        self.update(move |tray| {
            tray.state = if paused {
                TrayState::Paused
            } else {
                TrayState::Connected
            };
            tray.role_switching = false;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn audio_route(
        &self,
        choice: AudioChoice,
        local_available: bool,
        peer_available: bool,
    ) {
        self.update(move |tray| {
            tray.audio_choice = choice;
            tray.local_audio_available = local_available;
            tray.peer_audio_available = peer_available;
        })
        .await;
    }

    pub async fn role_assignment(
        &self,
        local_device_name: String,
        peer_device_name: String,
        local_is_controller: bool,
        available: bool,
    ) {
        self.update(|tray| {
            tray.local_device_name = Some(local_device_name);
            tray.peer_device_name = Some(peer_device_name);
            tray.local_is_controller = local_is_controller;
            tray.role_switch_available = available;
            tray.role_switching = false;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn role_switching(&self, switching: bool) {
        self.update(move |tray| tray.role_switching = switching)
            .await;
    }

    pub async fn role_failure(&self, error: String) {
        self.update(|tray| {
            tray.role_switching = false;
            tray.last_error = Some(error);
        })
        .await;
    }

    pub async fn error(&self, error: String) {
        self.set_round_trip(None);
        self.update(|tray| {
            tray.state = TrayState::Error;
            tray.connected_peer = None;
            tray.round_trip = None;
            tray.last_error = Some(error);
        })
        .await;
    }

    pub async fn phone_connected(
        &self,
        fingerprint: String,
        name: String,
        accepts_audio_invite: bool,
    ) {
        self.update(move |tray| {
            tray.phones.retain(|phone| phone.fingerprint != fingerprint);
            tray.phones.push(TrayPhone {
                fingerprint,
                name,
                round_trip: None,
                audio: None,
                accepts_audio_invite,
            });
            tray.phones.sort_by(|a, b| a.name.cmp(&b.name));
        })
        .await;
    }

    pub async fn phone_round_trip(&self, fingerprint: String, round_trip: Duration) {
        self.update(move |tray| {
            if let Some(phone) = tray.phone_mut(&fingerprint) {
                phone.round_trip = Some(round_trip);
            }
        })
        .await;
    }

    pub async fn phone_audio(&self, fingerprint: String, audio: Option<AudioDirection>) {
        self.update(move |tray| {
            if let Some(phone) = tray.phone_mut(&fingerprint) {
                phone.audio = audio;
            }
        })
        .await;
    }

    pub async fn phone_disconnected(&self, fingerprint: String) {
        self.update(move |tray| tray.phones.retain(|phone| phone.fingerprint != fingerprint))
            .await;
    }

    pub async fn shutdown(&self) {
        self.handle.shutdown().await;
    }

    async fn update(&self, update: impl FnOnce(&mut ReceiverTray)) {
        apply(&self.handle, &self.status, update).await;
    }
}

/// Changes the tray and republishes its status.
async fn apply(
    handle: &ksni::Handle<ReceiverTray>,
    status: &watch::Sender<TrayStatus>,
    update: impl FnOnce(&mut ReceiverTray),
) -> bool {
    let changed = handle.update(|tray| {
        update(tray);
        let next = tray.status();
        status.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    });
    time::timeout(TRAY_UPDATE_TIMEOUT, changed).await.is_ok()
}

fn spawn_live_updates(
    handle: ksni::Handle<ReceiverTray>,
    live: Arc<LiveCounters>,
    status: Arc<watch::Sender<TrayStatus>>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(LIVE_UPDATE_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !live.dirty.swap(false, Ordering::AcqRel) {
                continue;
            }
            let input_events = live.input_events.load(Ordering::Relaxed);
            let clipboard_events = live.clipboard_events.load(Ordering::Relaxed);
            let round_trip = live.round_trip();
            let applied = apply(&handle, &status, move |tray| {
                tray.input_events = input_events;
                tray.clipboard_events = clipboard_events;
                if tray.state == TrayState::Connected {
                    tray.round_trip = round_trip;
                }
            })
            .await;
            if !applied {
                live.dirty.store(true, Ordering::Release);
            }
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayState {
    Starting,
    Listening,
    Connected,
    Paused,
    Error,
}

#[derive(Debug)]
pub struct ReceiverTray {
    state: TrayState,
    transport: String,
    backend: String,
    pairing_armed: bool,
    connected_peer: Option<String>,
    round_trip: Option<Duration>,
    connections: u64,
    input_events: u64,
    clipboard_events: u64,
    input_forwarding_enabled: bool,
    audio_choice: AudioChoice,
    local_audio_available: bool,
    peer_audio_available: bool,
    local_device_name: Option<String>,
    peer_device_name: Option<String>,
    local_is_controller: bool,
    role_switch_available: bool,
    role_switching: bool,
    last_error: Option<String>,
    phones: Vec<TrayPhone>,
    /// Set once phone sessions run, which also shows "Add phone…".
    phone_requests: Option<mpsc::UnboundedSender<PhoneRequest>>,
    command_tx: mpsc::UnboundedSender<TrayCommand>,
}

impl ksni::Tray for ReceiverTray {
    fn id(&self) -> String {
        "edge-kvm-node".to_string()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }

    fn title(&self) -> String {
        format!("edge-kvm: {}", self.summary())
    }

    fn status(&self) -> ksni::Status {
        match self.state {
            TrayState::Error => ksni::Status::NeedsAttention,
            TrayState::Paused => ksni::Status::Passive,
            TrayState::Starting | TrayState::Listening | TrayState::Connected => {
                ksni::Status::Active
            }
        }
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        mouse_icons(self.icon_color())
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: self.icon_pixmap(),
            title: "edge-kvm".to_string(),
            description: self.description(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.command_tx.send(TrayCommand::OpenSettings);
    }

    /// Status first, then everyday actions. Rows that only apply sometimes
    /// are hidden rather than removed, so the layout only changes when a
    /// phone connects or leaves.
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let connected = self.state == TrayState::Connected;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![disabled_item(self.summary())];
        items.extend(
            self.phones
                .iter()
                .map(|phone| disabled_item(phone_line(phone))),
        );
        items.push(MenuItem::Separator);
        items.push(
            CheckmarkItem {
                label: "Share mouse and keyboard".to_string(),
                enabled: connected,
                checked: self.input_forwarding_enabled,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::ToggleInputForwarding);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(self.control_item());
        items.push(MenuItem::Separator);
        items.push(disabled_item("Audio".to_string()));
        items.push(self.audio_items());
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Add phone…".to_string(),
                visible: self.phone_requests.is_some(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::AddPhone);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Pair a computer…".to_string(),
                visible: !connected,
                enabled: !self.pairing_armed,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::ArmPairing);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(self.connection_item());
        items.push(
            StandardItem {
                label: "Settings…".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::OpenSettings);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

impl ReceiverTray {
    fn new(
        transport: String,
        backend: String,
        pairing_armed: bool,
        command_tx: mpsc::UnboundedSender<TrayCommand>,
    ) -> Self {
        Self {
            state: TrayState::Starting,
            transport,
            backend,
            pairing_armed,
            connected_peer: None,
            round_trip: None,
            connections: 0,
            input_events: 0,
            clipboard_events: 0,
            input_forwarding_enabled: true,
            audio_choice: AudioChoice::Off,
            local_audio_available: false,
            peer_audio_available: false,
            local_device_name: None,
            peer_device_name: None,
            local_is_controller: true,
            role_switch_available: false,
            role_switching: false,
            last_error: None,
            phones: Vec::new(),
            phone_requests: None,
            command_tx,
        }
    }

    fn phone_mut(&mut self, fingerprint: &str) -> Option<&mut TrayPhone> {
        self.phones
            .iter_mut()
            .find(|phone| phone.fingerprint == fingerprint)
    }

    fn peer_name(&self) -> &str {
        self.peer_device_name
            .as_deref()
            .or(self.connected_peer.as_deref())
            .unwrap_or("the other computer")
    }

    /// One line on what's going on, with the round trip while connected.
    fn summary(&self) -> String {
        let peer = self.peer_name();
        let summary = match self.state {
            TrayState::Starting => "Connecting…".to_string(),
            TrayState::Listening if self.pairing_armed => {
                "Ready to pair: confirm on both computers".to_string()
            }
            TrayState::Listening => "Waiting for a computer".to_string(),
            TrayState::Connected if self.role_switching => {
                format!("Switching control with {peer}…")
            }
            TrayState::Connected if self.local_is_controller => format!("Controlling {peer}"),
            TrayState::Connected => format!("{peer} is controlling this computer"),
            TrayState::Paused => "Disconnected".to_string(),
            TrayState::Error => format!(
                "Error: {}",
                shorten(self.last_error.as_deref().unwrap_or("unknown"), 60)
            ),
        };
        match self.round_trip {
            Some(round_trip) if self.state == TrayState::Connected => {
                format!("{summary} · {}", format_round_trip(round_trip))
            }
            _ => summary,
        }
    }

    fn status(&self) -> TrayStatus {
        let connected = self.state == TrayState::Connected;
        TrayStatus {
            summary: self.summary(),
            connected,
            peer: connected.then(|| self.peer_name().to_string()),
            round_trip_ms: self
                .round_trip
                .filter(|_| connected)
                .map(|rtt| u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX)),
            diagnostics: vec![
                ("transport", self.transport.clone()),
                ("input backend", self.backend.clone()),
                ("connections", self.connections.to_string()),
                ("input events", self.input_events.to_string()),
                ("clipboard events", self.clipboard_events.to_string()),
                ("phones", self.phones.len().to_string()),
                (
                    "last error",
                    self.last_error
                        .clone()
                        .unwrap_or_else(|| "none".to_string()),
                ),
            ],
        }
    }

    /// Hands control the other way. Hidden while it isn't possible.
    fn control_item(&self) -> ksni::MenuItem<Self> {
        let peer = self.peer_name();
        let (label, choice) = if self.local_is_controller {
            (
                format!("Let {peer} control this computer"),
                ControllerChoice::Peer,
            )
        } else {
            (format!("Control {peer} from here"), ControllerChoice::Local)
        };
        ksni::menu::StandardItem {
            label,
            visible: self.state == TrayState::Connected && self.role_switch_available,
            enabled: !self.role_switching,
            activate: Box::new(move |tray: &mut Self| {
                let _ = tray.command_tx.send(TrayCommand::SetController(choice));
            }),
            ..Default::default()
        }
        .into()
    }

    /// Every place audio can go, in menu order.
    fn audio_targets(&self) -> Vec<AudioTarget> {
        let mut targets = vec![
            AudioTarget::Pc(AudioChoice::Local),
            AudioTarget::Pc(AudioChoice::Peer),
        ];
        for phone in &self.phones {
            targets.push(AudioTarget::ToPhone(phone.fingerprint.clone()));
            targets.push(AudioTarget::FromPhone(phone.fingerprint.clone()));
        }
        targets.push(AudioTarget::Off);
        targets
    }

    fn current_audio_target(&self) -> AudioTarget {
        let phone = self
            .phones
            .iter()
            .find_map(|phone| Some((phone.fingerprint.clone(), phone.audio?)));
        match (phone, self.audio_choice) {
            (Some((fingerprint, AudioDirection::ToPhone)), _) => AudioTarget::ToPhone(fingerprint),
            (Some((fingerprint, AudioDirection::FromPhone)), _) => {
                AudioTarget::FromPhone(fingerprint)
            }
            (None, AudioChoice::Off) => AudioTarget::Off,
            (None, choice) => AudioTarget::Pc(choice),
        }
    }

    fn audio_items(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::{RadioGroup, RadioItem};

        let peer = self.peer_name();
        let connected = self.state == TrayState::Connected;
        let options = self
            .audio_targets()
            .into_iter()
            .map(|target| match &target {
                AudioTarget::Pc(AudioChoice::Local) => RadioItem {
                    label: format!("Send to {peer}"),
                    enabled: connected && self.local_audio_available,
                    ..Default::default()
                },
                AudioTarget::Pc(_) => RadioItem {
                    label: format!("Play from {peer}"),
                    enabled: connected && self.peer_audio_available,
                    ..Default::default()
                },
                AudioTarget::ToPhone(fingerprint) => {
                    let phone = self.phone(fingerprint);
                    RadioItem {
                        label: if phone.accepts_audio_invite {
                            format!("Send to {}", phone.name)
                        } else {
                            format!("Send to {} (update HyprMobile)", phone.name)
                        },
                        enabled: phone.accepts_audio_invite,
                        ..Default::default()
                    }
                }
                AudioTarget::FromPhone(fingerprint) => {
                    let phone = self.phone(fingerprint);
                    RadioItem {
                        label: format!("Play from {}", phone.name),
                        visible: phone.audio == Some(AudioDirection::FromPhone),
                        ..Default::default()
                    }
                }
                AudioTarget::Off => RadioItem {
                    label: "Off".to_string(),
                    ..Default::default()
                },
            })
            .collect();
        let current = self.current_audio_target();
        RadioGroup {
            selected: self
                .audio_targets()
                .iter()
                .position(|target| *target == current)
                .unwrap_or_default(),
            select: Box::new(|tray: &mut Self, selected| {
                if let Some(target) = tray.audio_targets().into_iter().nth(selected) {
                    tray.choose_audio(target);
                }
            }),
            options,
        }
        .into()
    }

    fn phone(&self, fingerprint: &str) -> &TrayPhone {
        self.phones
            .iter()
            .find(|phone| phone.fingerprint == fingerprint)
            .expect("audio targets come from the phone list")
    }

    /// Stops whatever else is playing, then starts `target`.
    fn choose_audio(&mut self, target: AudioTarget) {
        for phone in &self.phones {
            let keep = matches!(
                &target,
                AudioTarget::ToPhone(fingerprint) | AudioTarget::FromPhone(fingerprint)
                    if *fingerprint == phone.fingerprint
            );
            if phone.audio.is_some() && !keep {
                self.phone_audio_request(&phone.fingerprint, None);
            }
        }
        match target {
            AudioTarget::Pc(choice) => {
                let _ = self.command_tx.send(TrayCommand::SetAudio(choice));
            }
            AudioTarget::ToPhone(fingerprint) => {
                if self.audio_choice != AudioChoice::Off {
                    let _ = self
                        .command_tx
                        .send(TrayCommand::SetAudio(AudioChoice::Off));
                }
                self.phone_audio_request(&fingerprint, Some(AudioDirection::ToPhone));
            }
            // Already playing; only the phone can start it.
            AudioTarget::FromPhone(_) => {}
            AudioTarget::Off => {
                let _ = self
                    .command_tx
                    .send(TrayCommand::SetAudio(AudioChoice::Off));
            }
        }
    }

    fn phone_audio_request(&self, fingerprint: &str, direction: Option<AudioDirection>) {
        if let Some(requests) = &self.phone_requests {
            let _ = requests.send(PhoneRequest::Audio {
                fingerprint: fingerprint.to_string(),
                direction,
            });
        }
    }

    fn connection_item(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::StandardItem;

        let (label, command): (&str, fn() -> TrayCommand) = match self.state {
            TrayState::Paused => ("Reconnect", || TrayCommand::Reconnect),
            TrayState::Starting
            | TrayState::Listening
            | TrayState::Connected
            | TrayState::Error => ("Disconnect", || TrayCommand::Disconnect),
        };
        StandardItem {
            label: label.to_string(),
            activate: Box::new(move |tray: &mut Self| {
                let _ = tray.command_tx.send(command());
            }),
            ..Default::default()
        }
        .into()
    }

    fn icon_color(&self) -> IconColor {
        match self.state {
            TrayState::Connected => IconColor::Connected,
            TrayState::Starting | TrayState::Listening => IconColor::Connecting,
            TrayState::Paused | TrayState::Error => IconColor::Disconnected,
        }
    }

    fn description(&self) -> String {
        std::iter::once(self.summary())
            .chain(self.phones.iter().map(phone_line))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn phone_line(phone: &TrayPhone) -> String {
    match phone.round_trip {
        Some(round_trip) => format!("{} · {}", phone.name, format_round_trip(round_trip)),
        None => format!("{} · connected", phone.name),
    }
}

fn format_round_trip(round_trip: Duration) -> String {
    match round_trip.as_millis() {
        0 => "<1 ms".to_string(),
        millis => format!("{millis} ms"),
    }
}

fn shorten(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[derive(Clone, Copy)]
enum IconColor {
    Connecting,
    Connected,
    Disconnected,
}

fn mouse_icons(color: IconColor) -> Vec<ksni::Icon> {
    [22, 32]
        .into_iter()
        .map(|size| mouse_icon(size, color))
        .collect()
}

fn mouse_icon(size: i32, color: IconColor) -> ksni::Icon {
    let fill = match color {
        IconColor::Connecting => [0x9c, 0xa3, 0xaf],
        IconColor::Connected => [0x22, 0xc5, 0x5e],
        IconColor::Disconnected => [0xef, 0x44, 0x44],
    };
    let outline = [0x11, 0x18, 0x27];
    let highlight = [0xff, 0xff, 0xff];
    let mut data = vec![0; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let nx = (f64::from(x) + 0.5) / f64::from(size);
            let ny = (f64::from(y) + 0.5) / f64::from(size);
            let idx = ((y * size + x) * 4) as usize;

            let body = ellipse(nx, ny, 0.5, 0.56, 0.30, 0.39);
            let top = ellipse(nx, ny, 0.5, 0.30, 0.24, 0.20);
            let silhouette = body || top;
            if !silhouette {
                continue;
            }

            let border = !ellipse(nx, ny, 0.5, 0.56, 0.25, 0.34)
                || (top && !ellipse(nx, ny, 0.5, 0.30, 0.19, 0.15));
            let split = ny < 0.43 && (nx - 0.5).abs() < 0.018;
            let wheel = ellipse(nx, ny, 0.5, 0.34, 0.035, 0.075);
            let upper_highlight = ellipse(nx, ny, 0.41, 0.28, 0.055, 0.035);

            let (alpha, rgb) = if border || split {
                (0xee, outline)
            } else if wheel || upper_highlight {
                (0xd8, highlight)
            } else {
                (0xff, fill)
            };

            data[idx] = alpha;
            data[idx + 1] = rgb[0];
            data[idx + 2] = rgb[1];
            data[idx + 3] = rgb[2];
        }
    }

    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

fn ellipse(x: f64, y: f64, cx: f64, cy: f64, rx: f64, ry: f64) -> bool {
    let dx = (x - cx) / rx;
    let dy = (y - cy) / ry;
    dx * dx + dy * dy <= 1.0
}

fn disabled_item(label: String) -> ksni::MenuItem<ReceiverTray> {
    ksni::menu::StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;

    fn test_tray(state: TrayState) -> (ReceiverTray, mpsc::UnboundedReceiver<TrayCommand>) {
        let (command_tx, commands) = mpsc::unbounded_channel();
        let mut tray = ReceiverTray::new(
            "Listen: 0.0.0.0:42420".to_string(),
            "hyprland".to_string(),
            false,
            command_tx,
        );
        tray.state = state;
        tray.local_device_name = Some("Lua".to_string());
        tray.peer_device_name = Some("Main PC".to_string());
        tray.local_is_controller = false;
        tray.role_switch_available = true;
        tray.local_audio_available = true;
        tray.peer_audio_available = true;
        (tray, commands)
    }

    fn with_phone(
        tray: &mut ReceiverTray,
        accepts_audio_invite: bool,
    ) -> mpsc::UnboundedReceiver<PhoneRequest> {
        let (requests, received) = mpsc::unbounded_channel();
        tray.phone_requests = Some(requests);
        tray.phones.push(TrayPhone {
            fingerprint: "phone-key".to_string(),
            name: "CMF Phone 2 Pro".to_string(),
            round_trip: Some(Duration::from_millis(38)),
            audio: None,
            accepts_audio_invite,
        });
        received
    }

    fn labels(tray: &ReceiverTray) -> Vec<String> {
        tray.menu()
            .iter()
            .map(|item| match item {
                ksni::MenuItem::Standard(item) if item.visible => item.label.clone(),
                ksni::MenuItem::Standard(item) => format!("({})", item.label),
                ksni::MenuItem::Checkmark(item) => {
                    format!("[{}] {}", if item.checked { "x" } else { " " }, item.label)
                }
                ksni::MenuItem::RadioGroup(group) => group
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(_, option)| option.visible)
                    .map(|(index, option)| {
                        format!(
                            "{} {}",
                            if index == group.selected {
                                "(o)"
                            } else {
                                "( )"
                            },
                            option.label
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | "),
                ksni::MenuItem::Separator => "---".to_string(),
                ksni::MenuItem::SubMenu(_) => "submenu".to_string(),
            })
            .collect()
    }

    fn audio_group(tray: &ReceiverTray) -> ksni::menu::RadioGroup<ReceiverTray> {
        tray.menu()
            .into_iter()
            .find_map(|item| match item {
                ksni::MenuItem::RadioGroup(group) => Some(group),
                _ => None,
            })
            .expect("menu has an audio group")
    }

    #[test]
    fn connected_menu_leads_with_status_and_ping() {
        let (mut tray, _) = test_tray(TrayState::Connected);
        tray.round_trip = Some(Duration::from_millis(4));
        let _requests = with_phone(&mut tray, true);
        assert_eq!(
            labels(&tray),
            [
                "Main PC is controlling this computer · 4 ms",
                "CMF Phone 2 Pro · 38 ms",
                "---",
                "[x] Share mouse and keyboard",
                "Control Main PC from here",
                "---",
                "Audio",
                "( ) Send to Main PC | ( ) Play from Main PC | ( ) Send to CMF Phone 2 Pro | (o) Off",
                "---",
                "Add phone…",
                "(Pair a computer…)",
                "Disconnect",
                "Settings…",
                "Quit",
            ]
        );
    }

    #[test]
    fn layout_only_changes_with_phones() {
        let shape = |tray: &ReceiverTray| {
            tray.menu()
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>()
        };
        let (connected, _) = test_tray(TrayState::Connected);
        let expected = shape(&connected);
        for state in [
            TrayState::Starting,
            TrayState::Listening,
            TrayState::Paused,
            TrayState::Error,
        ] {
            assert_eq!(shape(&test_tray(state).0), expected, "{state:?}");
        }
    }

    #[test]
    fn status_line_says_who_is_in_control() {
        let (mut tray, _) = test_tray(TrayState::Connected);
        tray.local_is_controller = true;
        assert_eq!(tray.summary(), "Controlling Main PC");
        tray.state = TrayState::Listening;
        tray.round_trip = Some(Duration::from_millis(4));
        assert_eq!(tray.summary(), "Waiting for a computer");
        tray.pairing_armed = true;
        assert_eq!(tray.summary(), "Ready to pair: confirm on both computers");
        tray.state = TrayState::Error;
        tray.last_error = Some("connection lost".to_string());
        assert_eq!(tray.summary(), "Error: connection lost");
        assert_eq!(format_round_trip(Duration::from_micros(300)), "<1 ms");
    }

    #[test]
    fn diagnostics_live_in_the_status_not_the_menu() {
        let (mut tray, _) = test_tray(TrayState::Connected);
        tray.input_events = 182_344;
        assert!(!labels(&tray).iter().any(|label| label.contains("182344")));
        let status = tray.status();
        assert!(status.connected);
        assert_eq!(status.peer.as_deref(), Some("Main PC"));
        assert!(
            status
                .diagnostics
                .contains(&("input events", "182344".to_string()))
        );
    }

    #[test]
    fn sending_audio_to_a_phone_stops_pc_audio_first() {
        let (mut tray, mut commands) = test_tray(TrayState::Connected);
        let mut requests = with_phone(&mut tray, true);
        tray.audio_choice = AudioChoice::Local;
        let group = audio_group(&tray);
        assert_eq!(group.selected, 0);

        (group.select)(&mut tray, 2);
        assert!(matches!(
            commands.try_recv(),
            Ok(TrayCommand::SetAudio(AudioChoice::Off))
        ));
        assert!(matches!(
            requests.try_recv(),
            Ok(PhoneRequest::Audio { fingerprint, direction: Some(AudioDirection::ToPhone) })
                if fingerprint == "phone-key"
        ));
    }

    #[test]
    fn choosing_pc_audio_stops_the_phone_stream() {
        let (mut tray, mut commands) = test_tray(TrayState::Connected);
        let mut requests = with_phone(&mut tray, true);
        tray.phones[0].audio = Some(AudioDirection::FromPhone);
        let group = audio_group(&tray);
        // Phone → computer shows while it plays, and is the current choice.
        assert!(group.options[3].visible);
        assert_eq!(group.selected, 3);

        (group.select)(&mut tray, 1);
        assert!(matches!(
            requests.try_recv(),
            Ok(PhoneRequest::Audio {
                direction: None,
                ..
            })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(TrayCommand::SetAudio(AudioChoice::Peer))
        ));
    }

    #[test]
    fn phones_without_audio_invites_cannot_be_sent_audio() {
        let (mut tray, _) = test_tray(TrayState::Connected);
        let _requests = with_phone(&mut tray, false);
        let group = audio_group(&tray);
        assert_eq!(
            group.options[2].label,
            "Send to CMF Phone 2 Pro (update HyprMobile)"
        );
        assert!(!group.options[2].enabled);
    }

    #[test]
    fn control_item_hands_control_the_other_way() {
        let (mut tray, mut commands) = test_tray(TrayState::Connected);
        let menu = tray.menu();
        let ksni::MenuItem::Standard(control) = &menu[3] else {
            panic!("control item is not a standard item");
        };
        assert_eq!(control.label, "Control Main PC from here");
        (control.activate)(&mut tray);
        assert!(matches!(
            commands.try_recv(),
            Ok(TrayCommand::SetController(ControllerChoice::Local))
        ));

        tray.role_switch_available = false;
        let menu = tray.menu();
        let ksni::MenuItem::Standard(control) = &menu[3] else {
            panic!("control item is not a standard item");
        };
        assert!(!control.visible);
    }

    #[test]
    fn connection_action_matches_tray_state() {
        let label = |state| {
            let (tray, _) = test_tray(state);
            labels(&tray)[10].clone()
        };
        assert_eq!(label(TrayState::Connected), "Disconnect");
        assert_eq!(label(TrayState::Paused), "Reconnect");
        assert_eq!(label(TrayState::Listening), "Disconnect");
    }
}
