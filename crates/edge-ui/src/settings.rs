//! The settings window: one compact page, grouped into sections.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use edge_common::{
    AppConfig, AudioLocalPlayback, AudioRoutePreference, GameCompatibilityMode, PeerPosition, Role,
    TransportMode, parse_listen_port, update_listen_port, validate_device_name, validate_host,
    validate_port,
};
use eframe::egui::{self, Align, Frame, Layout, Margin, RichText, Ui};

use crate::{
    NodeLink, NodeStatus, PairingUiState, PhoneEntry, SettingsUiInput, SettingsUiResult,
    phone::{PairingAction, PhonePairing},
    widgets::{
        BAD, BAR, FAINT, LINE, MUTED, OK, primary_button, row_label, section, segmented,
        status_dot, toggle,
    },
};

const LABEL_WIDTH: f32 = 128.0;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const AUDIO_BUFFERS_MS: [u32; 5] = [40, 60, 80, 120, 160];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clipboard {
    Off,
    Text,
    TextAndImages,
}

/// Everything the page edits. Saving writes these into the config.
#[derive(Debug, Clone, PartialEq)]
struct Editor {
    device_name: String,
    /// Optional for a node: with an address it dials that computer, and
    /// either way it accepts computers and phones that dial it.
    peer_host: String,
    port: String,
    position: PeerPosition,
    game_compatibility: GameCompatibilityMode,
    clipboard: Clipboard,
    forward_phone_clipboard: bool,
    audio_route: AudioRoutePreference,
    audio_play_local: bool,
    jitter_target_ms: u32,
    notifications: edge_common::NotificationConfig,
}

impl Editor {
    fn from_config(config: &AppConfig) -> Self {
        // Both computers use one port: the one this node listens on and the
        // one it dials.
        let port = match config.transport {
            TransportMode::Connect => config.peer.port,
            TransportMode::Listen => config
                .listen
                .as_deref()
                .and_then(|listen| parse_listen_port(listen).ok())
                .unwrap_or(42_420),
        };
        Self {
            device_name: config.device_name.clone(),
            peer_host: config.peer.host.clone(),
            port: port.to_string(),
            position: config.layout.listener_position,
            game_compatibility: config.input.capture.game_compatibility,
            clipboard: match (config.clipboard.enabled, config.clipboard.images_enabled) {
                (false, _) => Clipboard::Off,
                (true, false) => Clipboard::Text,
                (true, true) => Clipboard::TextAndImages,
            },
            forward_phone_clipboard: config.phone.forward_clipboard_to_pcs,
            audio_route: config.audio.route.unwrap_or(if config.audio.enabled {
                AudioRoutePreference::PeerToLocal
            } else {
                AudioRoutePreference::Disabled
            }),
            audio_play_local: config.audio.local_playback == AudioLocalPlayback::Mirror,
            jitter_target_ms: config.audio.jitter_target_ms,
            notifications: config.notifications.clone(),
        }
    }

    /// `connector_only` is for the Windows app, which can only dial, so it
    /// needs an address and always connects.
    fn apply(&self, original: &AppConfig, connector_only: bool) -> Result<AppConfig> {
        validate_device_name(&self.device_name)?;
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .context("port must be a number between 1 and 65535")?;
        validate_port(port)?;

        let mut config = original.clone();
        config.device_name = self.device_name.trim().to_string();
        config.input.capture.game_compatibility = self.game_compatibility;
        config.clipboard.enabled = self.clipboard != Clipboard::Off;
        config.clipboard.images_enabled = self.clipboard == Clipboard::TextAndImages;
        config.phone.forward_clipboard_to_pcs = self.forward_phone_clipboard;
        config.audio.route = Some(self.audio_route);
        config.audio.enabled = self.audio_route != AudioRoutePreference::Disabled;
        config.audio.local_playback = if self.audio_play_local {
            AudioLocalPlayback::Mirror
        } else {
            AudioLocalPlayback::Redirect
        };
        config.audio.jitter_target_ms = self.jitter_target_ms;
        config.notifications = self.notifications.clone();
        let host = self.peer_host.trim();
        if connector_only || !host.is_empty() {
            validate_host(host)?;
        }
        config.peer.host = host.to_string();
        config.peer.port = port;
        config.layout.listener_position = self.position;
        if !connector_only {
            config.listen = Some(update_listen_port(config.listen.as_deref(), port));
        }
        // Kept for older builds and the Windows app; a Linux node listens
        // either way and dials whenever it has an address.
        config.transport = if connector_only || !host.is_empty() {
            TransportMode::Connect
        } else {
            TransportMode::Listen
        };
        Ok(config)
    }
}

/// What the running node last reported.
#[derive(Default)]
struct Live {
    polled: Option<Instant>,
    status: Option<NodeStatus>,
    phones: Option<Vec<PhoneEntry>>,
    autostart: Option<bool>,
    notifications: Option<crate::NotificationSetup>,
}

pub(crate) struct SettingsApp {
    role: Role,
    config_path: PathBuf,
    original: AppConfig,
    saved: Editor,
    editor: Editor,
    local_ip: String,
    pairing: PairingUiState,
    connector_only: bool,
    restart_on_save: bool,
    link: Option<Arc<dyn NodeLink>>,
    live: Live,
    phone_pairing: Option<PhonePairing>,
    confirm_remove: Option<String>,
    message: Option<(String, bool)>,
    shell_path: String,
    result: Arc<Mutex<SettingsUiResult>>,
}

impl SettingsApp {
    pub(crate) fn new(input: SettingsUiInput, result: Arc<Mutex<SettingsUiResult>>) -> Self {
        let editor = Editor::from_config(&input.config);
        Self {
            role: input.role,
            config_path: input.config_path,
            saved: editor.clone(),
            editor,
            local_ip: input
                .local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown address".to_string()),
            pairing: input.pairing,
            connector_only: input.connector_only,
            restart_on_save: input.restart_on_save,
            link: input.link,
            live: Live::default(),
            phone_pairing: None,
            confirm_remove: None,
            message: None,
            shell_path: String::new(),
            original: input.config,
            result,
        }
    }

    fn peer_name(&self) -> String {
        self.live
            .status
            .as_ref()
            .and_then(|status| status.peer.clone())
            .or_else(|| Some(self.original.peer.name.clone()).filter(|name| !name.is_empty()))
            .unwrap_or_else(|| "the other computer".to_string())
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(link) = &self.link else { return };
        if self
            .live
            .polled
            .is_some_and(|polled| polled.elapsed() < POLL_INTERVAL)
        {
            return;
        }
        self.live.polled = Some(Instant::now());
        self.live.status = link.status();
        self.live.phones = link.phones();
        self.live.autostart = link.autostart();
        self.live.notifications = link.notifications();
        if self.shell_path.is_empty()
            && let Some(path) = self
                .live
                .notifications
                .as_ref()
                .and_then(|s| s.shell_path.as_ref())
        {
            self.shell_path = path.display().to_string();
        }
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    fn save(&mut self) -> bool {
        if self.editor.notifications != self.saved.notifications
            && let Err(error) = validate_notification_selection(
                &self.editor.notifications,
                self.live.notifications.as_ref(),
            )
        {
            self.message = Some((error.to_string(), true));
            return false;
        }
        if self.editor.notifications.enabled
            && self.editor.notifications != self.saved.notifications
        {
            let installed = self
                .link
                .as_ref()
                .context("Connect to edge-kvm before enabling notifications")
                .and_then(|link| {
                    anyhow::ensure!(
                        !self.shell_path.is_empty(),
                        "Choose your Quickshell shell.qml so the bridge can be installed"
                    );
                    link.install_notification_bridge(std::path::Path::new(&self.shell_path))
                });
            if let Err(error) = installed {
                self.message = Some((format!("{error:#}"), true));
                return false;
            }
        }
        match self
            .editor
            .apply(&self.original, self.connector_only)
            .and_then(|config| {
                config
                    .save_blocking(&self.config_path)
                    .map(|()| config)
                    .map_err(Into::into)
            }) {
            Ok(config) => {
                self.original = config.clone();
                self.saved = self.editor.clone();
                self.message = Some((
                    if self.restart_on_save {
                        "Saved. Restarting edge-kvm…".to_string()
                    } else {
                        "Saved. Connection changes apply on reconnect.".to_string()
                    },
                    false,
                ));
                if let Ok(mut result) = self.result.lock() {
                    *result = SettingsUiResult::Saved(Box::new(config));
                }
                true
            }
            Err(error) => {
                self.message = Some((format!("{error:#}"), true));
                false
            }
        }
    }

    fn report(&self) -> String {
        let mut report = format!(
            "edge-kvm {}\nrole: {:?}\nconfig: {}\n",
            env!("CARGO_PKG_VERSION"),
            self.role,
            self.config_path.display()
        );
        match &self.live.status {
            Some(status) => {
                report.push_str(&format!("status: {}\n", status.summary));
                for (key, value) in &status.diagnostics {
                    report.push_str(&format!("{key}: {value}\n"));
                }
            }
            None => report.push_str("status: not reachable\n"),
        }
        if let Some(notifications) = &self.live.notifications {
            report.push_str(&format!("notifications: {}\n", notifications.status));
        }
        if let Some(phones) = &self.live.phones {
            for phone in phones {
                report.push_str(&format!(
                    "phone: {} ({})\n",
                    phone.name,
                    if phone.connected {
                        "connected"
                    } else {
                        "disconnected"
                    }
                ));
            }
        }
        report
    }

    fn status_bar(&self, ui: &mut Ui) {
        let (connected, summary) = match &self.live.status {
            Some(status) => (status.connected, status.summary.clone()),
            None if self.link.is_some() => (false, "edge-kvm isn't running".to_string()),
            None => (false, pairing_summary(&self.pairing)),
        };
        ui.horizontal(|ui| {
            status_dot(ui, if connected { OK } else { FAINT });
            ui.label(summary);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{}:{}", self.local_ip, self.editor.port.trim()))
                        .small()
                        .color(FAINT),
                );
            });
        });
    }

    fn this_computer(&mut self, ui: &mut Ui) {
        section(ui, "This computer");
        grid(ui, "this_computer", |ui| {
            row_label(ui, "Name");
            ui.add(
                egui::TextEdit::singleline(&mut self.editor.device_name)
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            row_label(ui, "Other computer");
            ui.add(
                egui::TextEdit::singleline(&mut self.editor.peer_host)
                    .hint_text(if self.connector_only {
                        "192.168.0.10"
                    } else {
                        "Address, or empty to let it find this one"
                    })
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            row_label(ui, "It sits to the");
            segmented(
                ui,
                &mut self.editor.position,
                &[
                    (PeerPosition::Left, "Left"),
                    (PeerPosition::Right, "Right"),
                    (PeerPosition::Top, "Top"),
                    (PeerPosition::Bottom, "Bottom"),
                ],
            );
            ui.end_row();

            row_label(ui, "Port");
            ui.add(egui::TextEdit::singleline(&mut self.editor.port).desired_width(64.0));
            ui.end_row();

            if let (Some(link), Some(mut enabled)) = (&self.link, self.live.autostart) {
                row_label(ui, "Start at login");
                if toggle(ui, &mut enabled).changed() {
                    match link.set_autostart(enabled) {
                        Ok(()) => self.live.autostart = Some(enabled),
                        Err(error) => self.message = Some((format!("{error:#}"), true)),
                    }
                }
                ui.end_row();
            }
        });
    }

    fn devices(&mut self, ui: &mut Ui) {
        section(ui, "Devices");
        let peer = self.peer_name();
        let connected = self
            .live
            .status
            .as_ref()
            .is_some_and(|status| status.connected);
        grid(ui, "devices", |ui| {
            row_label(ui, "Computer");
            ui.horizontal(|ui| {
                let ping = self
                    .live
                    .status
                    .as_ref()
                    .and_then(|status| status.round_trip_ms)
                    .map(|ms| format!(" · {ms} ms"))
                    .unwrap_or_default();
                ui.label(peer.clone());
                ui.label(
                    RichText::new(if connected { format!("connected{ping}") } else { "not connected".into() })
                        .color(FAINT),
                );
                if let Some(link) = &self.link {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let pair = ui
                            .add_enabled(!connected, egui::Button::new("Pair another…").small())
                            .on_disabled_hover_text("Disconnect from the tray first");
                        if pair.clicked() {
                            self.message = Some(match link.arm_pairing() {
                                Ok(()) => (
                                    "Ready to pair: start pairing on the other computer, then confirm on both.".into(),
                                    false,
                                ),
                                Err(error) => (format!("{error:#}"), true),
                            });
                        }
                    });
                }
            });
            ui.end_row();

            if !matches!(self.pairing, PairingUiState::Idle) {
                ui.label("");
                ui.label(
                    RichText::new(pairing_summary(&self.pairing))
                        .small()
                        .color(FAINT),
                );
                ui.end_row();
            }

            let Some(phones) = self.live.phones.clone() else {
                return;
            };
            row_label(ui, "Phones");
            ui.vertical(|ui| {
                if phones.is_empty() {
                    ui.label(RichText::new("None paired").color(FAINT));
                }
                for phone in &phones {
                    ui.horizontal(|ui| {
                        ui.label(phone.name.clone());
                        ui.label(
                            RichText::new(if phone.connected { "connected" } else { "away" })
                                .color(FAINT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.confirm_remove.as_deref() == Some(phone.name.as_str()) {
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("Remove").color(BAD))
                                            .small(),
                                    )
                                    .clicked()
                                {
                                    self.confirm_remove = None;
                                    if let Some(link) = &self.link {
                                        match link.unpair_phone(&phone.name) {
                                            Ok(()) => self.live.polled = None,
                                            Err(error) => {
                                                self.message = Some((format!("{error:#}"), true));
                                            }
                                        }
                                    }
                                }
                                if ui.small_button("Keep").clicked() {
                                    self.confirm_remove = None;
                                }
                            } else if ui.small_button("Remove…").clicked() {
                                self.confirm_remove = Some(phone.name.clone());
                            }
                        });
                    });
                }
                if primary_button(ui, "+ Add phone").clicked()
                    && let Some(link) = &self.link
                {
                    self.phone_pairing = Some(PhonePairing::start(link.as_ref()));
                }
            });
            ui.end_row();
        });
    }

    fn sharing(&mut self, ui: &mut Ui) {
        section(ui, "Sharing");
        let peer = short_name(&self.peer_name());
        let phones_supported = self.live.phones.is_some();
        grid(ui, "sharing", |ui| {
            row_label(ui, "Clipboard");
            segmented(
                ui,
                &mut self.editor.clipboard,
                &[
                    (Clipboard::Off, "Off"),
                    (Clipboard::Text, "Text"),
                    (Clipboard::TextAndImages, "Text + images"),
                ],
            );
            ui.end_row();

            if phones_supported {
                row_label(ui, "Phone clipboard");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.editor.clipboard != Clipboard::Off, |ui| {
                        toggle(ui, &mut self.editor.forward_phone_clipboard);
                    });
                    ui.label(
                        RichText::new(format!("also send to {peer}"))
                            .small()
                            .color(FAINT),
                    );
                });
                ui.end_row();
            }

            row_label(ui, "Audio");
            let to_peer = format!("To {peer}");
            let from_peer = format!("From {peer}");
            segmented(
                ui,
                &mut self.editor.audio_route,
                &[
                    (AudioRoutePreference::LocalToPeer, to_peer.as_str()),
                    (AudioRoutePreference::PeerToLocal, from_peer.as_str()),
                    (AudioRoutePreference::Disabled, "Off"),
                ],
            );
            ui.end_row();

            row_label(ui, "Play here too");
            ui.add_enabled_ui(
                self.editor.audio_route == AudioRoutePreference::LocalToPeer,
                |ui| {
                    toggle(ui, &mut self.editor.audio_play_local);
                },
            );
            ui.end_row();
        });
        if phones_supported {
            ui.label(
                RichText::new("Send audio to a phone from the tray menu.")
                    .small()
                    .color(FAINT),
            );
        }
    }

    fn notification_sharing(&mut self, ui: &mut Ui) {
        if self.connector_only {
            return;
        }
        section(ui, "Notifications");
        let Some(setup) = self.live.notifications.clone() else {
            ui.label(
                RichText::new(
                    "Update and restart edge-kvm to configure notification sharing here.",
                )
                .small()
                .color(FAINT),
            );
            return;
        };
        let active_peer = self.live.status.as_ref().and_then(|s| s.peer.as_deref());
        let local_name = self.editor.device_name.clone();
        grid(ui, "notification_sharing", |ui| {
            row_label(ui, "Shared history");
            let config = &mut self.editor.notifications;
            if toggle(ui, &mut config.enabled).changed() && config.enabled {
                select_notification_peer(config, &setup, active_peer);
            }
            ui.end_row();

            if config.enabled {
                row_label(ui, "Share with");
                let choices = notification_choices(&setup, active_peer, &config.peers);
                ui.vertical(|ui| {
                    if choices.is_empty() {
                        ui.label("Connect or choose a saved pairing below.");
                    }
                    for computer in &choices {
                        let mut enrolled = config.peers.contains(&computer.fingerprint);
                        let label = if Some(computer.name.as_str()) == active_peer {
                            format!("{} · connected", computer.name)
                        } else {
                            computer.name.clone()
                        };
                        if ui.checkbox(&mut enrolled, label).changed() {
                            if enrolled {
                                config.peers.push(computer.fingerprint.clone());
                            } else {
                                config.peers.retain(|p| p != &computer.fingerprint);
                            }
                        }
                    }
                    let others: Vec<_> = setup
                        .computers
                        .iter()
                        .filter(|p| !choices.iter().any(|c| c.fingerprint == p.fingerprint))
                        .collect();
                    if !others.is_empty() {
                        ui.collapsing(format!("Other saved pairings ({})", others.len()), |ui| {
                            for computer in others {
                                if ui
                                    .small_button(format!(
                                        "Add {} (offline pairing)",
                                        computer.name
                                    ))
                                    .clicked()
                                {
                                    config.peers.push(computer.fingerprint.clone());
                                }
                            }
                        });
                    }
                });
                ui.end_row();

                row_label(ui, "History stored on");
                let owner = if config.coordinator_fingerprint == setup.identity {
                    format!("{local_name} (this computer)")
                } else {
                    setup
                        .computers
                        .iter()
                        .find(|p| p.fingerprint == config.coordinator_fingerprint)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| "Choose a computer".into())
                };
                let previous_owner = config.coordinator_fingerprint.clone();
                ui.add_enabled_ui(!setup.has_store, |ui| {
                    egui::ComboBox::from_id_salt("notification_owner").selected_text(owner).show_ui(ui, |ui| {
                        ui.selectable_value(&mut config.coordinator_fingerprint, setup.identity.clone(), format!("{local_name} (this computer)"));
                        // The connected computer is offered immediately, even
                        // before selecting its sharing checkbox.
                        for computer in notification_choices(&setup, active_peer, &config.peers) {
                            ui.selectable_value(&mut config.coordinator_fingerprint, computer.fingerprint.clone(), &computer.name);
                        }
                    });
                }).response.on_hover_text("Choose the same computer on both PCs. The owner is fixed once history is created.");
                if previous_owner != config.coordinator_fingerprint
                    && config.coordinator_fingerprint != setup.identity
                    && !config.peers.contains(&config.coordinator_fingerprint)
                {
                    config.peers.push(config.coordinator_fingerprint.clone());
                }
                ui.end_row();
            }

            row_label(ui, "Quickshell");
            ui.horizontal(|ui| {
                if setup.shell_path.is_some() {
                    ui.label(if setup.bridge_installed {
                        "Bridge installed"
                    } else {
                        "Bridge installs on Save"
                    })
                    .on_hover_text(&self.shell_path);
                } else {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.shell_path)
                            .hint_text("Path to shell.qml")
                            .desired_width(180.0),
                    );
                }
                if setup.bridge_installed
                    && ui.small_button("Update bridge").clicked()
                    && let Some(link) = &self.link
                {
                    match link.install_notification_bridge(std::path::Path::new(&self.shell_path)) {
                        Ok(message) => {
                            self.message = Some((message, false));
                            self.live.polled = None;
                        }
                        Err(error) => self.message = Some((format!("{error:#}"), true)),
                    }
                }
            });
            ui.end_row();
            row_label(ui, "Status");
            ui.label(&setup.status);
            ui.end_row();
        });
        if self.editor.notifications.enabled {
            ui.label(
                RichText::new(
                    "Enable on both PCs and choose the same history owner. DND stays local.",
                )
                .small()
                .color(FAINT),
            );
        }
    }

    fn advanced(&mut self, ui: &mut Ui) {
        section(ui, "Advanced");
        grid(ui, "advanced", |ui| {
            row_label(ui, "Audio buffer");
            egui::ComboBox::from_id_salt("audio_buffer")
                .selected_text(format!("{} ms", self.editor.jitter_target_ms))
                .width(96.0)
                .show_ui(ui, |ui| {
                    for ms in AUDIO_BUFFERS_MS {
                        ui.selectable_value(
                            &mut self.editor.jitter_target_ms,
                            ms,
                            format!("{ms} ms"),
                        );
                    }
                });
            ui.end_row();

            if cfg!(windows) {
                row_label(ui, "Games");
                egui::ComboBox::from_id_salt("game_compatibility")
                    .selected_text(game_compatibility_label(self.editor.game_compatibility))
                    .show_ui(ui, |ui| {
                        for mode in [
                            GameCompatibilityMode::AlwaysEnabled,
                            GameCompatibilityMode::Borderless,
                            GameCompatibilityMode::Compatible,
                        ] {
                            ui.selectable_value(
                                &mut self.editor.game_compatibility,
                                mode,
                                game_compatibility_label(mode),
                            );
                        }
                    });
                ui.end_row();
            }

            row_label(ui, "Diagnostics");
            ui.horizontal(|ui| {
                if ui.small_button("Copy report").clicked() {
                    ui.ctx().copy_text(self.report());
                    self.message = Some(("Report copied.".to_string(), false));
                }
                if let Some(link) = &self.link
                    && ui.small_button("Open log").clicked()
                    && let Err(error) = link.open_log()
                {
                    self.message = Some((format!("{error:#}"), true));
                }
            });
            ui.end_row();
        });
    }

    fn footer(&mut self, ui: &mut Ui) {
        let dirty = self.editor != self.saved;
        ui.horizontal(|ui| {
            match &self.message {
                Some((message, true)) => {
                    ui.label(RichText::new(message).small().color(BAD));
                }
                Some((message, false)) => {
                    ui.label(RichText::new(message).small().color(MUTED));
                }
                None if dirty && self.restart_on_save => {
                    ui.label(
                        RichText::new("edge-kvm restarts to apply changes")
                            .small()
                            .color(FAINT),
                    );
                }
                None => {}
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let save = ui.add_enabled(
                    dirty,
                    egui::Button::new(RichText::new("Save").color(egui::Color32::WHITE))
                        .fill(if dirty { crate::widgets::ACCENT } else { LINE }),
                );
                if save.clicked() && self.save() && self.restart_on_save {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }

        self.poll(&ui.ctx().clone());
        let bar = Frame::new()
            .fill(BAR)
            .inner_margin(Margin::symmetric(16, 9));
        egui::Panel::top("status")
            .frame(bar)
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("footer")
            .frame(bar)
            .show(ui, |ui| self.footer(ui));
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(crate::widgets::WINDOW)
                    .inner_margin(Margin::symmetric(18, 4)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.this_computer(ui);
                        self.devices(ui);
                        self.sharing(ui);
                        self.notification_sharing(ui);
                        self.advanced(ui);
                        ui.add_space(8.0);
                    });
            });

        if let (Some(pairing), Some(link)) = (&mut self.phone_pairing, &self.link) {
            let link = Arc::clone(link);
            let modal = egui::Modal::new(egui::Id::new("add_phone")).show(ui.ctx(), |ui| {
                ui.set_width(300.0);
                pairing.ui(ui, link.as_ref())
            });
            if modal.inner == PairingAction::Close || modal.should_close() {
                if pairing.paired() {
                    self.live.polled = None;
                }
                self.phone_pairing = None;
            }
        }
    }
}

/// Two columns: muted labels on the left, controls on the right.
fn select_notification_peer(
    config: &mut edge_common::NotificationConfig,
    setup: &crate::NotificationSetup,
    active_peer: Option<&str>,
) {
    if config.peers.is_empty()
        && let Some(peer) = setup
            .computers
            .iter()
            .find(|p| Some(p.name.as_str()) == active_peer)
            .or_else(|| (setup.computers.len() == 1).then(|| &setup.computers[0]))
    {
        config.peers.push(peer.fingerprint.clone());
    }
    if config.coordinator_fingerprint.is_empty() {
        config.coordinator_fingerprint = config
            .peers
            .iter()
            .chain(std::iter::once(&setup.identity))
            .min()
            .cloned()
            .unwrap_or_default();
    }
}

fn notification_choices<'a>(
    setup: &'a crate::NotificationSetup,
    active_peer: Option<&str>,
    enrolled: &[String],
) -> Vec<&'a crate::NotificationComputer> {
    let mut choices: Vec<_> = setup
        .computers
        .iter()
        .filter(|p| Some(p.name.as_str()) == active_peer || enrolled.contains(&p.fingerprint))
        .collect();
    choices.sort_by_key(|p| (Some(p.name.as_str()) != active_peer, &p.name));
    choices.dedup_by(|a, b| a.fingerprint == b.fingerprint);
    choices
}

fn validate_notification_selection(
    config: &edge_common::NotificationConfig,
    setup: Option<&crate::NotificationSetup>,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let setup = setup.context("Connect to the running node before enabling notifications")?;
    anyhow::ensure!(
        !config.peers.is_empty(),
        "Choose a paired computer to share notifications with"
    );
    anyhow::ensure!(
        config
            .peers
            .iter()
            .all(|id| setup.computers.iter().any(|p| &p.fingerprint == id)),
        "Only paired computers can share notifications; remove any unpaired devices from the selection"
    );
    anyhow::ensure!(
        config.coordinator_fingerprint == setup.identity
            || config.peers.contains(&config.coordinator_fingerprint),
        "Choose an enrolled computer to store history"
    );
    anyhow::ensure!(
        !setup.has_store || config.coordinator_fingerprint == setup.coordinator,
        "History already belongs to another computer; changing its owner requires migration"
    );
    Ok(())
}

fn grid(ui: &mut Ui, id: &str, rows: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .min_col_width(LABEL_WIDTH)
        .spacing([14.0, 8.0])
        .show(ui, rows);
}

/// Keeps segment labels short for long computer names.
fn short_name(name: &str) -> String {
    const MAX: usize = 14;
    match name.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &name[..end]),
        None => name.to_string(),
    }
}

fn game_compatibility_label(mode: GameCompatibilityMode) -> &'static str {
    match mode {
        GameCompatibilityMode::AlwaysEnabled => "Always enabled",
        GameCompatibilityMode::Borderless => "Borderless games",
        GameCompatibilityMode::Compatible => "Safe / compatible",
    }
}

fn pairing_summary(pairing: &PairingUiState) -> String {
    match pairing {
        PairingUiState::Idle => "edge-kvm".to_string(),
        PairingUiState::PendingIncoming(pairing) => {
            format!("Pairing request from {}", pairing.peer_name)
        }
        PairingUiState::PendingOutgoing(pairing) => format!("Waiting for {}", pairing.peer_name),
        PairingUiState::Paired { peer_name, .. } => format!("Paired with {peer_name}"),
        PairingUiState::Error(error) => error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_round_trips_the_config() {
        let config = AppConfig::receiver_default();
        let editor = Editor::from_config(&config);
        let saved = editor.apply(&config, false).unwrap();
        assert_eq!(Editor::from_config(&saved), editor);
    }

    #[test]
    fn clipboard_choice_maps_onto_both_flags() {
        let config = AppConfig::receiver_default();
        let mut editor = Editor::from_config(&config);
        editor.clipboard = Clipboard::Text;
        let saved = editor.apply(&config, false).unwrap();
        assert!(saved.clipboard.enabled);
        assert!(!saved.clipboard.images_enabled);
        editor.clipboard = Clipboard::Off;
        assert!(!editor.apply(&config, false).unwrap().clipboard.enabled);
    }

    #[test]
    fn an_address_is_optional_and_decides_who_dials() {
        let config = AppConfig::receiver_default();
        let mut editor = Editor::from_config(&config);
        editor.peer_host.clear();
        editor.port = "42500".to_string();
        let waiting = editor.apply(&config, false).unwrap();
        assert_eq!(waiting.transport, TransportMode::Listen);
        assert_eq!(waiting.listen.as_deref(), Some("0.0.0.0:42500"));

        editor.peer_host = "192.168.0.9".to_string();
        let dialing = editor.apply(&config, false).unwrap();
        assert_eq!(dialing.transport, TransportMode::Connect);
        assert_eq!(dialing.peer.host, "192.168.0.9");
        assert_eq!(dialing.peer.port, 42_500);
        // Still listens, for phones and computers that dial it.
        assert_eq!(dialing.listen.as_deref(), Some("0.0.0.0:42500"));
    }

    #[test]
    fn the_windows_app_needs_an_address() {
        let config = AppConfig::receiver_default();
        let mut editor = Editor::from_config(&config);
        editor.peer_host.clear();
        assert!(editor.apply(&config, true).is_err());
    }

    struct TestNode {
        fail_install: bool,
        installed: Arc<std::sync::atomic::AtomicBool>,
    }
    impl NodeLink for TestNode {
        fn status(&self) -> Option<NodeStatus> {
            None
        }
        fn phones(&self) -> Option<Vec<PhoneEntry>> {
            None
        }
        fn unpair_phone(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn arm_pairing(&self) -> Result<()> {
            Ok(())
        }
        fn pair_phone(&self) -> Result<std::sync::mpsc::Receiver<crate::PhonePairingEvent>> {
            anyhow::bail!("unused")
        }
        fn autostart(&self) -> Option<bool> {
            None
        }
        fn set_autostart(&self, _: bool) -> Result<()> {
            Ok(())
        }
        fn open_log(&self) -> Result<()> {
            Ok(())
        }
        fn install_notification_bridge(&self, _: &std::path::Path) -> Result<String> {
            anyhow::ensure!(!self.fail_install, "Unsupported shell");
            self.installed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok("Installed".into())
        }
    }

    #[test]
    fn save_installs_the_bridge_and_refuses_to_enable_when_installation_fails() {
        for fail_install in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let config_path = dir.path().join("receiver.toml");
            let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let result = Arc::new(Mutex::new(SettingsUiResult::Cancelled));
            let mut app = SettingsApp::new(
                SettingsUiInput {
                    role: Role::Receiver,
                    connector_only: false,
                    restart_on_save: true,
                    config_path: config_path.clone(),
                    config: AppConfig::receiver_default(),
                    local_ip: None,
                    pairing: PairingUiState::Idle,
                    link: Some(Arc::new(TestNode {
                        fail_install,
                        installed: installed.clone(),
                    })),
                },
                result.clone(),
            );
            app.shell_path = dir.path().join("shell.qml").display().to_string();
            app.live.notifications = Some(crate::NotificationSetup {
                identity: "local".into(),
                computers: vec![crate::NotificationComputer {
                    name: "Main PC".into(),
                    fingerprint: "main".into(),
                }],
                ..Default::default()
            });
            app.editor.notifications = edge_common::NotificationConfig {
                enabled: true,
                coordinator_fingerprint: "main".into(),
                peers: vec!["main".into()],
            };
            assert_eq!(app.save(), !fail_install);
            assert_eq!(config_path.exists(), !fail_install);
            assert_eq!(
                installed.load(std::sync::atomic::Ordering::Relaxed),
                !fail_install
            );
            assert_eq!(
                matches!(*result.lock().unwrap(), SettingsUiResult::Saved(_)),
                !fail_install
            );
        }
    }

    #[test]
    fn connected_computer_is_selected_and_available_as_owner_without_enrollment() {
        let setup = crate::NotificationSetup {
            identity: "local".into(),
            computers: vec![
                crate::NotificationComputer {
                    name: "Main PC".into(),
                    fingerprint: "old-key".into(),
                },
                crate::NotificationComputer {
                    name: "Tera".into(),
                    fingerprint: "current-key".into(),
                },
            ],
            ..Default::default()
        };
        let choices = notification_choices(&setup, Some("Tera"), &[]);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].name, "Tera");
        let mut config = edge_common::NotificationConfig::default();
        select_notification_peer(&mut config, &setup, Some("Tera"));
        assert_eq!(config.peers, ["current-key"]);
        assert!(!config.peers.contains(&"old-key".to_string()));
    }

    #[test]
    fn sharing_requires_trusted_members_and_cannot_replace_an_existing_owner() {
        let mut setup = crate::NotificationSetup {
            identity: "local".into(),
            computers: vec![crate::NotificationComputer {
                name: "Main PC".into(),
                fingerprint: "main".into(),
            }],
            coordinator: "main".into(),
            ..Default::default()
        };
        let mut config = edge_common::NotificationConfig {
            enabled: true,
            coordinator_fingerprint: "main".into(),
            peers: vec!["main".into()],
        };
        assert!(validate_notification_selection(&config, Some(&setup)).is_ok());
        config.peers.push("stranger".into());
        assert!(validate_notification_selection(&config, Some(&setup)).is_err());
        config.peers.pop();
        setup.has_store = true;
        config.coordinator_fingerprint = "local".into();
        assert!(validate_notification_selection(&config, Some(&setup)).is_err());
        config.enabled = false;
        assert!(validate_notification_selection(&config, Some(&setup)).is_ok());
    }

    #[test]
    fn long_names_are_shortened_for_segments() {
        assert_eq!(short_name("Main PC"), "Main PC");
        assert_eq!(short_name("A very long computer name"), "A very long co…");
    }
}
