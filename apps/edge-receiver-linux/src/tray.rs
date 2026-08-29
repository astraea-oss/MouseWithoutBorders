use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ksni::TrayMethods;
use tokio::sync::{Mutex, mpsc};

const COUNTER_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum TrayCommand {
    OpenSettings,
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

#[derive(Clone)]
pub struct ReceiverTrayHandle {
    handle: ksni::Handle<ReceiverTray>,
    input_events: Arc<AtomicU64>,
    last_input_update: Arc<Mutex<Instant>>,
}

impl ReceiverTrayHandle {
    pub async fn spawn(
        transport: String,
        backend: String,
        pairing_armed: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TrayCommand>), ksni::Error> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let tray = ReceiverTray {
            state: TrayState::Starting,
            transport,
            backend,
            pairing_armed,
            connected_peer: None,
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
            command_tx,
        };
        let handle = tray.assume_sni_available(true).spawn().await?;
        Ok((
            Self {
                handle,
                input_events: Arc::new(AtomicU64::new(0)),
                last_input_update: Arc::new(Mutex::new(Instant::now() - COUNTER_UPDATE_INTERVAL)),
            },
            command_rx,
        ))
    }

    pub async fn listening(&self) {
        self.update(|tray| {
            tray.state = TrayState::Listening;
            tray.connected_peer = None;
            tray.last_error = None;
        })
        .await;
    }

    #[cfg(target_os = "linux")]
    pub async fn connecting(&self) {
        self.update(|tray| {
            tray.state = TrayState::Starting;
            tray.connected_peer = None;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn pairing_armed(&self, armed: bool) {
        self.update(move |tray| tray.pairing_armed = armed).await;
    }

    pub async fn connected(&self, peer: String) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = TrayState::Connected;
            tray.connected_peer = Some(peer);
            tray.connections = tray.connections.saturating_add(1);
            tray.input_events = input_events;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn disconnected(&self, error: Option<String>) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = if error.is_some() {
                TrayState::Error
            } else {
                TrayState::Listening
            };
            tray.connected_peer = None;
            tray.input_events = input_events;
            tray.last_error = error;
        })
        .await;
    }

    pub async fn disconnected_by_user(&self) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = TrayState::Paused;
            tray.connected_peer = None;
            tray.input_events = input_events;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn input_event(&self) {
        let total = self.input_events.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last_update = self.last_input_update.lock().await;
        if last_update.elapsed() < COUNTER_UPDATE_INTERVAL {
            return;
        }
        *last_update = Instant::now();
        drop(last_update);

        self.update(move |tray| tray.input_events = total).await;
    }

    pub async fn clipboard_event(&self) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.input_events = input_events;
            tray.clipboard_events = tray.clipboard_events.saturating_add(1);
        })
        .await;
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
        self.update(|tray| {
            tray.state = TrayState::Error;
            tray.connected_peer = None;
            tray.last_error = Some(error);
        })
        .await;
    }

    pub async fn shutdown(&self) {
        self.handle.shutdown().await;
    }

    async fn update(&self, update: impl FnOnce(&mut ReceiverTray)) {
        let _ = self.handle.update(update).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayState {
    Starting,
    Listening,
    Connected,
    Paused,
    Error,
}

impl TrayState {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Listening => "Listening",
            Self::Connected => "Connected",
            Self::Paused => "Disconnected",
            Self::Error => "Error",
        }
    }
}

#[derive(Debug)]
pub struct ReceiverTray {
    state: TrayState,
    transport: String,
    backend: String,
    pairing_armed: bool,
    connected_peer: Option<String>,
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
        format!("edge-kvm: {}", self.state.label())
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

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            disabled_item(format!("Status: {}", self.state.label())),
            disabled_item(format!("Transport: {}", self.transport)),
            disabled_item(format!("Input backend: {}", self.backend)),
            disabled_item(format!(
                "Pairing: {}",
                if self.pairing_armed {
                    "waiting for confirmation"
                } else {
                    "not armed"
                }
            )),
            disabled_item(format!(
                "Peer: {}",
                self.connected_peer.as_deref().unwrap_or("none")
            )),
            disabled_item(format!("Connections: {}", self.connections)),
            disabled_item(format!("Input events: {}", self.input_events)),
            disabled_item(format!("Clipboard events: {}", self.clipboard_events)),
            disabled_item(format!(
                "Last error: {}",
                self.last_error.as_deref().unwrap_or("None")
            )),
        ];

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: if self.pairing_armed {
                    "Pairing enabled for next connection".to_string()
                } else {
                    "Pair or replace peer...".to_string()
                },
                icon_name: "network-connect".to_string(),
                enabled: !self.pairing_armed && self.state != TrayState::Connected,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::ArmPairing);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(self.connection_item());
        items.push(self.controller_items());
        items.push(
            CheckmarkItem {
                label: "Forward mouse and keyboard".to_string(),
                icon_name: "input-mouse".to_string(),
                enabled: self.state == TrayState::Connected,
                checked: self.input_forwarding_enabled,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::ToggleInputForwarding);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(disabled_item("Audio routing".to_string()));
        items.push(self.audio_items());
        items.push(
            StandardItem {
                label: "Settings...".to_string(),
                icon_name: "preferences-system".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::OpenSettings);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit edge-kvm".to_string(),
                icon_name: "application-exit".to_string(),
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
    fn audio_items(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::{RadioGroup, RadioItem};

        let local = self.local_device_name.as_deref().unwrap_or("This computer");
        let peer = self.peer_device_name.as_deref().unwrap_or("Peer");
        let connected = self.state == TrayState::Connected;
        RadioGroup {
            selected: match self.audio_choice {
                AudioChoice::Off => 0,
                AudioChoice::Local => 1,
                AudioChoice::Peer => 2,
            },
            select: Box::new(|tray: &mut Self, selected| {
                let choice = match selected {
                    0 => AudioChoice::Off,
                    1 => AudioChoice::Local,
                    _ => AudioChoice::Peer,
                };
                let _ = tray.command_tx.send(TrayCommand::SetAudio(choice));
            }),
            options: vec![
                RadioItem {
                    label: "Audio off".to_string(),
                    enabled: true,
                    ..Default::default()
                },
                RadioItem {
                    label: format!("{local} → {peer}"),
                    enabled: connected && self.local_audio_available,
                    ..Default::default()
                },
                RadioItem {
                    label: format!("{peer} → {local}"),
                    enabled: connected && self.peer_audio_available,
                    ..Default::default()
                },
            ],
        }
        .into()
    }

    fn controller_items(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::{RadioGroup, RadioItem};

        let local = self.local_device_name.as_deref().unwrap_or("This computer");
        let peer = self.peer_device_name.as_deref().unwrap_or("Peer");
        let enabled = self.state == TrayState::Connected
            && self.role_switch_available
            && !self.role_switching;
        RadioGroup {
            selected: usize::from(!self.local_is_controller),
            select: Box::new(|tray: &mut Self, selected| {
                let choice = if selected == 0 {
                    ControllerChoice::Local
                } else {
                    ControllerChoice::Peer
                };
                let _ = tray.command_tx.send(TrayCommand::SetController(choice));
            }),
            options: vec![
                RadioItem {
                    label: format!("{local} controls {peer}"),
                    enabled,
                    ..Default::default()
                },
                RadioItem {
                    label: format!("{peer} controls {local}"),
                    enabled,
                    ..Default::default()
                },
            ],
        }
        .into()
    }

    fn connection_item(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::StandardItem;

        match self.state {
            TrayState::Connected => StandardItem {
                label: "Disconnect".to_string(),
                icon_name: "network-offline".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::Disconnect);
                }),
                ..Default::default()
            }
            .into(),
            TrayState::Paused => StandardItem {
                label: "Reconnect".to_string(),
                icon_name: "network-connect".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::Reconnect);
                }),
                ..Default::default()
            }
            .into(),
            TrayState::Starting | TrayState::Listening | TrayState::Error => StandardItem {
                label: "Disconnect".to_string(),
                icon_name: "network-offline".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::Disconnect);
                }),
                ..Default::default()
            }
            .into(),
        }
    }

    fn icon_color(&self) -> IconColor {
        match self.state {
            TrayState::Connected => IconColor::Connected,
            TrayState::Starting | TrayState::Listening => IconColor::Connecting,
            TrayState::Paused | TrayState::Error => IconColor::Disconnected,
        }
    }

    fn description(&self) -> String {
        let mut lines = vec![
            format!("Status: {}", self.state.label()),
            format!("Transport: {}", self.transport),
            format!("Input backend: {}", self.backend),
        ];
        if let Some(peer) = &self.connected_peer {
            lines.push(format!("Connected peer: {peer}"));
        }
        lines.push(format!("Connections: {}", self.connections));
        lines.push(format!("Input events: {}", self.input_events));
        lines.push(format!("Clipboard events: {}", self.clipboard_events));
        lines.push(format!(
            "Last error: {}",
            self.last_error.as_deref().unwrap_or("None")
        ));
        lines.join("\n")
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

    fn test_tray(state: TrayState, last_error: Option<&str>) -> ReceiverTray {
        let (command_tx, _) = mpsc::unbounded_channel();
        ReceiverTray {
            state,
            transport: "Listen: 0.0.0.0:42420".to_string(),
            backend: "hyprland".to_string(),
            pairing_armed: true,
            connected_peer: (state == TrayState::Connected).then(|| "controller".to_string()),
            connections: 1,
            input_events: 2,
            clipboard_events: 3,
            input_forwarding_enabled: true,
            audio_choice: AudioChoice::Off,
            local_audio_available: true,
            peer_audio_available: true,
            local_device_name: Some("Desk".to_string()),
            peer_device_name: Some("Studio".to_string()),
            local_is_controller: true,
            role_switch_available: true,
            role_switching: false,
            last_error: last_error.map(str::to_string),
            command_tx,
        }
    }

    fn menu_shape(tray: &ReceiverTray) -> Vec<&'static str> {
        tray.menu()
            .iter()
            .map(|item| match item {
                ksni::MenuItem::Standard(_) => "item",
                ksni::MenuItem::Separator => "separator",
                ksni::MenuItem::Checkmark(_) => "checkmark",
                ksni::MenuItem::SubMenu(_) => "submenu",
                ksni::MenuItem::RadioGroup(_) => "radio",
            })
            .collect()
    }

    fn menu_label(tray: &ReceiverTray, index: usize) -> String {
        match &tray.menu()[index] {
            ksni::MenuItem::Standard(item) => item.label.clone(),
            _ => panic!("menu item {index} is not a standard item"),
        }
    }

    #[test]
    fn menu_structure_stays_fixed_across_states_and_errors() {
        let connected = test_tray(TrayState::Connected, None);
        let listening = test_tray(TrayState::Listening, None);
        let paused = test_tray(TrayState::Paused, None);
        let error = test_tray(TrayState::Error, Some("connection lost"));

        let expected = menu_shape(&connected);
        assert_eq!(expected.len(), 19);
        assert_eq!(menu_shape(&listening), expected);
        assert_eq!(menu_shape(&paused), expected);
        assert_eq!(menu_shape(&error), expected);
        assert_eq!(menu_label(&connected, 8), "Last error: None");
        assert_eq!(menu_label(&error, 8), "Last error: connection lost");
    }

    #[test]
    fn connection_action_matches_tray_state() {
        assert_eq!(
            menu_label(&test_tray(TrayState::Connected, None), 11),
            "Disconnect"
        );
        assert_eq!(
            menu_label(&test_tray(TrayState::Paused, None), 11),
            "Reconnect"
        );
        assert_eq!(
            menu_label(&test_tray(TrayState::Listening, None), 11),
            "Disconnect"
        );
        assert_eq!(
            menu_label(&test_tray(TrayState::Error, Some("failed")), 11),
            "Disconnect"
        );
    }

    #[test]
    fn pairing_action_is_only_available_while_unarmed_and_disconnected() {
        let listening = test_tray(TrayState::Listening, None);
        let menu = listening.menu();
        let ksni::MenuItem::Standard(pairing) = &menu[10] else {
            panic!("pairing action is not a standard item");
        };
        assert_eq!(pairing.label, "Pairing enabled for next connection");
        assert!(!pairing.enabled);

        let mut listening = test_tray(TrayState::Listening, None);
        listening.pairing_armed = false;
        let menu = listening.menu();
        let ksni::MenuItem::Standard(pairing) = &menu[10] else {
            panic!("pairing action is not a standard item");
        };
        assert_eq!(pairing.label, "Pair or replace peer...");
        assert!(pairing.enabled);

        let mut connected = test_tray(TrayState::Connected, None);
        connected.pairing_armed = false;
        let menu = connected.menu();
        let ksni::MenuItem::Standard(pairing) = &menu[10] else {
            panic!("pairing action is not a standard item");
        };
        assert!(!pairing.enabled);
    }

    #[test]
    fn input_forwarding_action_reflects_runtime_state() {
        let mut connected = test_tray(TrayState::Connected, None);
        connected.input_forwarding_enabled = false;
        let menu = connected.menu();
        let ksni::MenuItem::Checkmark(toggle) = &menu[13] else {
            panic!("input forwarding action is not a checkmark");
        };
        assert_eq!(toggle.label, "Forward mouse and keyboard");
        assert!(toggle.enabled);
        assert!(!toggle.checked);

        let listening = test_tray(TrayState::Listening, None);
        let menu = listening.menu();
        let ksni::MenuItem::Checkmark(toggle) = &menu[13] else {
            panic!("input forwarding action is not a checkmark");
        };
        assert!(!toggle.enabled);
    }

    #[test]
    fn role_actions_use_device_names_and_committed_selection() {
        let tray = test_tray(TrayState::Connected, None);
        let menu = tray.menu();
        let ksni::MenuItem::RadioGroup(roles) = &menu[12] else {
            panic!("role actions are not a radio group");
        };
        assert_eq!(roles.selected, 0);
        assert_eq!(roles.options[0].label, "Desk controls Studio");
        assert_eq!(roles.options[1].label, "Studio controls Desk");
        assert!(roles.options.iter().all(|option| option.enabled));

        let mut switching = test_tray(TrayState::Connected, None);
        switching.local_is_controller = false;
        switching.role_switching = true;
        let ksni::MenuItem::RadioGroup(roles) = &switching.menu()[12] else {
            panic!("role actions are not a radio group");
        };
        assert_eq!(roles.selected, 1);
        assert!(roles.options.iter().all(|option| !option.enabled));
    }

    #[test]
    fn audio_actions_are_directional_named_and_capability_gated() {
        let mut tray = test_tray(TrayState::Connected, None);
        tray.audio_choice = AudioChoice::Peer;
        tray.local_audio_available = false;
        let menu = tray.menu();
        let ksni::MenuItem::Standard(heading) = &menu[15] else {
            panic!("audio heading is not a standard item");
        };
        assert_eq!(heading.label, "Audio routing");
        assert!(!heading.enabled);
        let ksni::MenuItem::RadioGroup(audio) = &menu[16] else {
            panic!("audio actions are not a radio group");
        };
        assert_eq!(audio.selected, 2);
        assert_eq!(audio.options[0].label, "Audio off");
        assert_eq!(audio.options[1].label, "Desk → Studio");
        assert_eq!(audio.options[2].label, "Studio → Desk");
        assert!(audio.options[0].enabled);
        assert!(!audio.options[1].enabled);
        assert!(audio.options[2].enabled);
    }
}
