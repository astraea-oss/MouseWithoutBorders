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
/// The ping shown in the menu changes at most this often.
const ROUND_TRIP_REFRESH: Duration = Duration::from_secs(5);

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
    round_trip_shown_at: std::sync::Mutex<Option<std::time::Instant>>,
    dirty: AtomicBool,
}

impl LiveCounters {
    fn new() -> Self {
        Self {
            input_events: AtomicU64::new(0),
            clipboard_events: AtomicU64::new(0),
            round_trip_micros: AtomicU64::new(NO_ROUND_TRIP),
            round_trip_shown_at: std::sync::Mutex::new(None),
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

    /// The heartbeat round trip to the PC peer. The first one shows at once;
    /// after that the menu changes at most every [`ROUND_TRIP_REFRESH`], and
    /// only when the shown milliseconds would change.
    pub fn peer_round_trip(&self, round_trip: Option<Duration>) {
        let shown = self.live.round_trip();
        if shown.map(|rtt| rtt.as_millis()) == round_trip.map(|rtt| rtt.as_millis()) {
            return;
        }
        let mut shown_at = self
            .live
            .round_trip_shown_at
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let due = shown.is_none() || shown_at.is_none_or(|at| at.elapsed() >= ROUND_TRIP_REFRESH);
        if due {
            *shown_at = Some(std::time::Instant::now());
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
        // The status rows open Settings, which also keeps them from being
        // greyed out like disabled rows.
        let summary = self.summary();
        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                icon_data: tagged_png(status_dot_png(self.icon_color()), &summary),
                label: summary,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::OpenSettings);
                }),
                ..Default::default()
            }
            .into(),
        ];
        items.extend(self.phones.iter().map(|phone| {
            let line = phone_line(phone);
            StandardItem {
                icon_data: tagged_png(menu_icon_png(MenuIcon::Phone), &line),
                label: line,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::OpenSettings);
                }),
                ..Default::default()
            }
            .into()
        }));
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
                icon_data: menu_icon_png(MenuIcon::Add).to_vec(),
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
                icon_data: menu_icon_png(MenuIcon::Pair).to_vec(),
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
                icon_data: menu_icon_png(MenuIcon::Settings).to_vec(),
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
                icon_data: menu_icon_png(MenuIcon::Quit).to_vec(),
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
            icon_data: menu_icon_png(MenuIcon::Swap).to_vec(),
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

        let (label, icon, command): (&str, MenuIcon, fn() -> TrayCommand) = match self.state {
            TrayState::Paused => ("Reconnect", MenuIcon::Pair, || TrayCommand::Reconnect),
            TrayState::Starting
            | TrayState::Listening
            | TrayState::Connected
            | TrayState::Error => ("Disconnect", MenuIcon::Disconnect, || {
                TrayCommand::Disconnect
            }),
        };
        StandardItem {
            label: label.to_string(),
            icon_data: menu_icon_png(icon).to_vec(),
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

/// A coloured dot for the status row, as PNG, drawn once per colour.
fn status_dot_png(color: IconColor) -> &'static [u8] {
    static DOTS: [std::sync::OnceLock<Vec<u8>>; 3] = [const { std::sync::OnceLock::new() }; 3];
    let (index, rgb) = match color {
        IconColor::Connected => (0, [0x3e, 0xcf, 0x8e]),
        IconColor::Connecting => (1, [0x9a, 0xa1, 0xad]),
        IconColor::Disconnected => (2, [0xf0, 0x71, 0x78]),
    };
    DOTS[index].get_or_init(|| {
        const SIZE: u32 = 16;
        const RADIUS: f32 = 4.5;
        let dot = image::RgbaImage::from_fn(SIZE, SIZE, |x, y| {
            let center = SIZE as f32 / 2.0;
            let distance =
                ((x as f32 + 0.5 - center).powi(2) + (y as f32 + 0.5 - center).powi(2)).sqrt();
            // One pixel of antialiasing at the edge.
            let alpha = (RADIUS + 0.5 - distance).clamp(0.0, 1.0);
            image::Rgba([rgb[0], rgb[1], rgb[2], (alpha * 255.0) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        dot.write_to(&mut png, image::ImageFormat::Png)
            .expect("a 16×16 PNG always encodes");
        png.into_inner()
    })
}

/// Menu icons are drawn here rather than named from the icon theme, so they
/// look the same everywhere instead of falling back to the theme's
/// missing-icon square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuIcon {
    Phone,
    Add,
    Swap,
    Pair,
    Disconnect,
    Settings,
    Quit,
}

const MENU_ICON_COUNT: usize = 7;

#[derive(Clone, Copy)]
enum Stroke {
    Line([f32; 2], [f32; 2]),
    /// A circle, open by `gap` radians either side of twelve o'clock.
    Ring {
        center: [f32; 2],
        radius: f32,
        gap: f32,
    },
}

impl Stroke {
    fn distance(&self, p: [f32; 2]) -> f32 {
        match *self {
            Stroke::Line(a, b) => {
                let ab = [b[0] - a[0], b[1] - a[1]];
                let ap = [p[0] - a[0], p[1] - a[1]];
                let length = ab[0] * ab[0] + ab[1] * ab[1];
                let t = ((ap[0] * ab[0] + ap[1] * ab[1]) / length).clamp(0.0, 1.0);
                (ap[0] - t * ab[0]).hypot(ap[1] - t * ab[1])
            }
            Stroke::Ring {
                center,
                radius,
                gap,
            } => {
                let (dx, dy) = (p[0] - center[0], p[1] - center[1]);
                let angle = dx.atan2(-dy);
                if angle.abs() >= gap {
                    return (dx.hypot(dy) - radius).abs();
                }
                let end = |angle: f32| {
                    let (x, y) = (radius * angle.sin(), -radius * angle.cos());
                    (dx - x).hypot(dy - y)
                };
                end(gap).min(end(-gap))
            }
        }
    }
}

impl MenuIcon {
    /// Strokes on a 16×16 grid.
    fn strokes(self) -> Vec<Stroke> {
        use Stroke::Line;
        let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
            [
                Line([x0, y0], [x1, y0]),
                Line([x1, y0], [x1, y1]),
                Line([x1, y1], [x0, y1]),
                Line([x0, y1], [x0, y0]),
            ]
        };
        match self {
            MenuIcon::Phone => {
                let mut strokes = rect(5.0, 1.0, 11.0, 14.0).to_vec();
                strokes.push(Line([7.0, 12.0], [9.0, 12.0]));
                strokes
            }
            MenuIcon::Add => vec![Line([8.0, 3.0], [8.0, 13.0]), Line([3.0, 8.0], [13.0, 8.0])],
            MenuIcon::Swap => vec![
                Line([2.0, 5.0], [13.0, 5.0]),
                Line([10.0, 2.0], [13.0, 5.0]),
                Line([10.0, 8.0], [13.0, 5.0]),
                Line([14.0, 11.0], [3.0, 11.0]),
                Line([6.0, 8.0], [3.0, 11.0]),
                Line([6.0, 14.0], [3.0, 11.0]),
            ],
            MenuIcon::Pair => {
                let mut strokes = rect(1.0, 2.0, 14.0, 10.0).to_vec();
                strokes.push(Line([8.0, 10.0], [8.0, 13.0]));
                strokes.push(Line([5.0, 13.0], [11.0, 13.0]));
                strokes
            }
            MenuIcon::Disconnect => {
                vec![
                    Line([4.0, 4.0], [12.0, 12.0]),
                    Line([12.0, 4.0], [4.0, 12.0]),
                ]
            }
            MenuIcon::Settings => {
                let mut strokes = vec![Stroke::Ring {
                    center: [8.0, 8.0],
                    radius: 3.8,
                    gap: 0.0,
                }];
                strokes.extend((0..8).map(|tooth| {
                    let angle = tooth as f32 * std::f32::consts::FRAC_PI_4;
                    let (sin, cos) = angle.sin_cos();
                    Line(
                        [8.0 + 4.5 * cos, 8.0 + 4.5 * sin],
                        [8.0 + 6.3 * cos, 8.0 + 6.3 * sin],
                    )
                }));
                strokes
            }
            MenuIcon::Quit => vec![
                Stroke::Ring {
                    center: [8.0, 8.0],
                    radius: 5.5,
                    gap: 0.6,
                },
                Line([8.0, 1.0], [8.0, 7.0]),
            ],
        }
    }
}

/// A light-grey line icon, as PNG, drawn once per icon.
fn menu_icon_png(icon: MenuIcon) -> &'static [u8] {
    static ICONS: [std::sync::OnceLock<Vec<u8>>; MENU_ICON_COUNT] =
        [const { std::sync::OnceLock::new() }; MENU_ICON_COUNT];
    ICONS[icon as usize].get_or_init(|| {
        const SIZE: u32 = 16;
        const HALF_WIDTH: f32 = 0.75;
        let strokes = icon.strokes();
        let image = image::RgbaImage::from_fn(SIZE, SIZE, |x, y| {
            // Pixel centres sit on whole coordinates, so whole-number strokes are crisp.
            let p = [x as f32, y as f32];
            let distance = strokes
                .iter()
                .map(|stroke| stroke.distance(p))
                .fold(f32::INFINITY, f32::min);
            let alpha = (HALF_WIDTH + 0.5 - distance).clamp(0.0, 1.0);
            image::Rgba([0xc8, 0xcc, 0xd4, (alpha * 255.0) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("a 16×16 PNG always encodes");
        png.into_inner()
    })
}

/// The icon with the row's label written into it as a PNG text chunk.
///
/// Quickshell (0.3.2) applies a property update as if it listed every
/// property, so changing just a row's label clears its icon. With the label
/// inside the icon, a new label always arrives with the icon alongside it.
fn tagged_png(png: &[u8], label: &str) -> Vec<u8> {
    const IEND_LEN: usize = 12;
    let (body, end) = png.split_at(png.len() - IEND_LEN);
    let mut chunk = b"tEXt".to_vec();
    chunk.extend_from_slice(b"Comment\0");
    chunk.extend_from_slice(label.as_bytes());
    let mut tagged = body.to_vec();
    tagged.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
    tagged.extend_from_slice(&chunk);
    tagged.extend_from_slice(&crc32fast::hash(&chunk).to_be_bytes());
    tagged.extend_from_slice(end);
    tagged
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
    fn status_row_shows_a_coloured_dot_and_opens_settings() {
        let (mut tray, mut commands) = test_tray(TrayState::Connected);
        let menu = tray.menu();
        let ksni::MenuItem::Standard(status) = &menu[0] else {
            panic!("status row is not a standard item");
        };
        assert!(status.enabled);
        let mut slower = test_tray(TrayState::Connected).0;
        slower.round_trip = Some(Duration::from_millis(40));
        let ksni::MenuItem::Standard(slower) = &slower.menu()[0] else {
            panic!("status row is not a standard item");
        };
        assert_ne!(
            status.icon_data, slower.icon_data,
            "a new label must bring the icon with it"
        );
        let dot = image::load_from_memory(&status.icon_data)
            .unwrap()
            .to_rgba8();
        assert_eq!(dot.get_pixel(8, 8).0, [0x3e, 0xcf, 0x8e, 0xff]);
        assert_eq!(dot.get_pixel(0, 0).0[3], 0);
        (status.activate)(&mut tray);
        assert!(matches!(commands.try_recv(), Ok(TrayCommand::OpenSettings)));
    }

    #[test]
    fn menu_icons_are_drawn_not_named_from_the_theme() {
        let (tray, _) = test_tray(TrayState::Connected);
        for item in tray.menu() {
            if let ksni::MenuItem::Standard(item) = item {
                assert!(
                    item.icon_name.is_empty(),
                    "{} names a theme icon",
                    item.label
                );
            }
        }
        for icon in [
            MenuIcon::Phone,
            MenuIcon::Add,
            MenuIcon::Swap,
            MenuIcon::Pair,
            MenuIcon::Disconnect,
            MenuIcon::Settings,
            MenuIcon::Quit,
        ] {
            let image = image::load_from_memory(menu_icon_png(icon))
                .unwrap()
                .to_rgba8();
            assert!(
                image.pixels().any(|pixel| pixel.0[3] == 0xff),
                "{icon:?} draws nothing"
            );
        }
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
