use std::{
    fs::OpenOptions,
    io::Write,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use anyhow::{Context, Result};
use edge_common::{
    AppConfig, AudioLocalPlayback, GameCompatibilityMode, PeerPosition, Role, parse_listen_port,
    update_listen_port, validate_device_name, validate_host, validate_port,
};

static SETTINGS_WINDOW_OPEN: OnceLock<Mutex<bool>> = OnceLock::new();
static SETTINGS_CONTEXT: OnceLock<Mutex<Option<eframe::egui::Context>>> = OnceLock::new();

pub struct SettingsUiInput {
    pub role: Role,
    pub config_path: PathBuf,
    pub config: AppConfig,
    pub local_ip: Option<IpAddr>,
    pub pairing: PairingUiState,
}

#[derive(Debug, Clone)]
pub struct PairingConfirmationInput {
    pub peer_name: String,
    pub peer_addr: Option<String>,
    pub local_fingerprint: String,
    pub peer_fingerprint: String,
    pub verification_code: String,
    pub previous_peer_fingerprint: Option<String>,
}

pub fn run_pairing_confirmation(input: PairingConfirmationInput) -> Result<bool> {
    let accepted = Arc::new(Mutex::new(false));
    let app_accepted = Arc::clone(&accepted);
    let mut options = eframe::NativeOptions {
        persist_window: false,
        persistence_path: None,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([520.0, 420.0])
            .with_resizable(false),
        ..Default::default()
    };
    configure_event_loop(&mut options);

    eframe::run_native(
        "Confirm edge-kvm pairing",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            Ok(Box::new(PairingConfirmationApp {
                input,
                accepted: app_accepted,
            }))
        }),
    )
    .map_err(|err| anyhow::anyhow!("failed to run pairing confirmation: {err}"))?;

    Ok(*accepted
        .lock()
        .map_err(|_| anyhow::anyhow!("pairing confirmation lock poisoned"))?)
}

struct PairingConfirmationApp {
    input: PairingConfirmationInput,
    accepted: Arc<Mutex<bool>>,
}

impl eframe::App for PairingConfirmationApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        use eframe::egui::{self, Align, Layout, RichText};

        ui.heading("Confirm this connection");
        ui.add_space(8.0);
        ui.label(format!("Pair with {}?", self.input.peer_name));
        if let Some(addr) = &self.input.peer_addr {
            ui.label(format!("Network address: {addr}"));
        }
        if self.input.previous_peer_fingerprint.is_some() {
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(248, 180, 80),
                "The saved identity key changed. Only continue if you intentionally reset or reinstalled the other computer.",
            );
        }
        ui.add_space(12.0);
        ui.label("Verify that this code is identical on both computers:");
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(&self.input.verification_code)
                    .monospace()
                    .size(38.0),
            );
        });
        ui.add_space(8.0);
        ui.collapsing("Identity details", |ui| {
            ui.label("This computer:");
            ui.monospace(&self.input.local_fingerprint);
            ui.label("Other computer:");
            ui.monospace(&self.input.peer_fingerprint);
            if let Some(previous) = &self.input.previous_peer_fingerprint {
                ui.label("Previously saved identity:");
                ui.monospace(previous);
            }
        });
        ui.add_space(16.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Pair").clicked() {
                if let Ok(mut accepted) = self.accepted.lock() {
                    *accepted = true;
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if ui.button("Cancel").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

#[derive(Debug, Clone)]
pub enum SettingsUiResult {
    Saved(Box<AppConfig>),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingUiState {
    Idle,
    PendingIncoming(PendingPairing),
    PendingOutgoing(PendingPairing),
    Paired {
        peer_name: String,
        peer_fingerprint: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPairing {
    pub peer_name: String,
    pub peer_fingerprint: String,
    pub peer_addr: Option<String>,
    pub started_at: SystemTime,
}

pub fn spawn_settings_window(input: SettingsUiInput) {
    let guard = SETTINGS_WINDOW_OPEN.get_or_init(|| Mutex::new(false));
    {
        let mut open = guard.lock().expect("settings window guard poisoned");
        if *open {
            if let Some(context) = SETTINGS_CONTEXT
                .get_or_init(|| Mutex::new(None))
                .lock()
                .ok()
                .and_then(|context| context.clone())
            {
                context.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
                context.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            }
            return;
        }
        *open = true;
    }

    std::thread::spawn(move || {
        let error_log = input
            .config_path
            .parent()
            .map(|parent| parent.join("state").join("settings.log"));
        let window_result = std::panic::catch_unwind(|| run_settings_window(input));
        let error = match window_result {
            Ok(Ok(_)) => None,
            Ok(Err(err)) => Some(format!("{err:#}")),
            Err(panic) => Some(format!(
                "settings window panicked: {}",
                panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic")
            )),
        };
        if let Some(error) = error {
            tracing::error!(%error, "settings window exited with an error");
            if let Some(path) = error_log {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(file, "{error}");
                }
            }
        }
        if let Ok(mut context) = SETTINGS_CONTEXT.get_or_init(|| Mutex::new(None)).lock() {
            *context = None;
        }
        if let Ok(mut open) = SETTINGS_WINDOW_OPEN
            .get_or_init(|| Mutex::new(false))
            .lock()
        {
            *open = false;
        }
    });
}

pub fn run_settings_window(input: SettingsUiInput) -> Result<SettingsUiResult> {
    let result = Arc::new(Mutex::new(SettingsUiResult::Cancelled));
    let app_result = Arc::clone(&result);
    let mut options = eframe::NativeOptions {
        persist_window: false,
        persistence_path: None,
        ..Default::default()
    };
    configure_event_loop(&mut options);

    eframe::run_native(
        "edge-kvm Settings",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            if let Ok(mut context) = SETTINGS_CONTEXT.get_or_init(|| Mutex::new(None)).lock() {
                *context = Some(cc.egui_ctx.clone());
            }
            Ok(Box::new(SettingsApp::new(input, app_result)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("failed to run settings window: {err}"))?;

    let result = result
        .lock()
        .map_err(|_| anyhow::anyhow!("settings result lock poisoned"))?
        .clone();
    Ok(result)
}

fn configure_event_loop(options: &mut eframe::NativeOptions) {
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;

        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_any_thread(true);
        }));
    }
    #[cfg(target_os = "linux")]
    {
        use winit::platform::{wayland::EventLoopBuilderExtWayland, x11::EventLoopBuilderExtX11};

        options.event_loop_builder = Some(Box::new(|builder| {
            EventLoopBuilderExtWayland::with_any_thread(builder, true);
            EventLoopBuilderExtX11::with_any_thread(builder, true);
        }));
    }
}

struct SettingsApp {
    role: Role,
    config_path: PathBuf,
    original: AppConfig,
    local_ip: String,
    pairing: PairingUiState,
    device_name: String,
    peer_host: String,
    port: String,
    position: PeerPosition,
    game_compatibility: GameCompatibilityMode,
    clipboard_images_enabled: bool,
    audio_enabled: bool,
    audio_play_local: bool,
    save_message: Option<String>,
    error_message: Option<String>,
    result: Arc<Mutex<SettingsUiResult>>,
}

impl SettingsApp {
    fn new(input: SettingsUiInput, result: Arc<Mutex<SettingsUiResult>>) -> Self {
        let peer = input.config.peer.clone();
        let port = match input.role {
            Role::Controller => peer.port,
            Role::Receiver => input
                .config
                .listen
                .as_deref()
                .and_then(|listen| parse_listen_port(listen).ok())
                .unwrap_or(42_420),
        };
        Self {
            role: input.role,
            config_path: input.config_path,
            local_ip: input
                .local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "Unknown".to_string()),
            pairing: input.pairing,
            device_name: input.config.device_name.clone(),
            peer_host: peer.host,
            port: port.to_string(),
            position: input.config.layout.listener_position,
            game_compatibility: input.config.input.capture.game_compatibility,
            clipboard_images_enabled: input.config.clipboard.images_enabled,
            audio_enabled: input.config.audio.enabled,
            audio_play_local: input.config.audio.local_playback == AudioLocalPlayback::Mirror,
            original: input.config,
            save_message: None,
            error_message: None,
            result,
        }
    }

    fn save(&mut self) {
        self.error_message = None;
        self.save_message = None;

        match self.edited_config() {
            Ok(config) => match config.save_blocking(&self.config_path) {
                Ok(()) => {
                    self.original = config.clone();
                    self.save_message = Some(
                        "Saved. Audio changes apply immediately; connection changes apply on reconnect."
                            .to_string(),
                    );
                    if let Ok(mut result) = self.result.lock() {
                        *result = SettingsUiResult::Saved(Box::new(config));
                    }
                }
                Err(err) => self.error_message = Some(err.to_string()),
            },
            Err(err) => self.error_message = Some(err.to_string()),
        }
    }

    fn edited_config(&self) -> Result<AppConfig> {
        validate_device_name(&self.device_name)?;
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .context("port must be a number between 1 and 65535")?;
        validate_port(port)?;

        let mut config = self.original.clone();
        config.device_name = self.device_name.trim().to_string();
        config.input.capture.game_compatibility = self.game_compatibility;
        config.clipboard.images_enabled = self.clipboard_images_enabled;
        config.audio.enabled = self.audio_enabled;
        config.audio.local_playback = if self.audio_play_local {
            AudioLocalPlayback::Mirror
        } else {
            AudioLocalPlayback::Redirect
        };

        match self.role {
            Role::Controller => {
                validate_host(&self.peer_host)?;
                config.peer.host = self.peer_host.trim().to_string();
                config.peer.port = port;
                config.layout.listener_position = self.position;
            }
            Role::Receiver => {
                config.listen = Some(update_listen_port(config.listen.as_deref(), port));
            }
        }

        Ok(config)
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        use eframe::egui::{self, Align, Layout};

        #[cfg(windows)]
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }

        ui.heading("edge-kvm Settings");
        ui.add_space(8.0);

        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([18.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut self.device_name);
                ui.end_row();

                ui.label("Game compatibility");
                if self.role == Role::Controller {
                    egui::ComboBox::from_id_salt("game_compatibility")
                        .selected_text(game_compatibility_label(self.game_compatibility))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.game_compatibility,
                                GameCompatibilityMode::AlwaysEnabled,
                                "Always enabled",
                            );
                            ui.selectable_value(
                                &mut self.game_compatibility,
                                GameCompatibilityMode::Borderless,
                                "Borderless games",
                            );
                            ui.selectable_value(
                                &mut self.game_compatibility,
                                GameCompatibilityMode::Compatible,
                                "Safe / compatible",
                            );
                        });
                } else {
                    let mut text = "Set on controller".to_string();
                    ui.add_enabled(false, egui::TextEdit::singleline(&mut text));
                }
                ui.end_row();

                ui.label("Local IP");
                ui.add_enabled(false, egui::TextEdit::singleline(&mut self.local_ip));
                ui.end_row();

                ui.label("Peer IP");
                if self.role == Role::Controller {
                    ui.text_edit_singleline(&mut self.peer_host);
                } else {
                    let mut receiver_peer = "Not used by receiver".to_string();
                    ui.add_enabled(false, egui::TextEdit::singleline(&mut receiver_peer));
                }
                ui.end_row();

                ui.label("Port");
                ui.text_edit_singleline(&mut self.port);
                ui.end_row();

                ui.label("Screen location");
                if self.role == Role::Controller {
                    egui::ComboBox::from_id_salt("screen_location")
                        .selected_text(position_label(self.position))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.position, PeerPosition::Left, "Left");
                            ui.selectable_value(&mut self.position, PeerPosition::Right, "Right");
                            ui.selectable_value(&mut self.position, PeerPosition::Top, "Top");
                            ui.selectable_value(&mut self.position, PeerPosition::Bottom, "Bottom");
                        });
                } else {
                    let mut text = "Set on controller".to_string();
                    ui.add_enabled(false, egui::TextEdit::singleline(&mut text));
                }
                ui.end_row();

                ui.label("Pairing status");
                ui.label(pairing_text(&self.pairing));
                ui.end_row();

                ui.label("Clipboard images");
                ui.checkbox(
                    &mut self.clipboard_images_enabled,
                    "Sync images on connection",
                );
                ui.end_row();

                ui.label("Stream Linux audio");
                ui.checkbox(&mut self.audio_enabled, "Enabled on connection");
                ui.end_row();

                ui.label("Laptop playback");
                ui.checkbox(&mut self.audio_play_local, "Keep playing locally");
                ui.end_row();

                ui.label("Windows output");
                let mut output = "Follow system default".to_string();
                ui.add_enabled(false, egui::TextEdit::singleline(&mut output));
                ui.end_row();
            });

        ui.add_space(12.0);
        if let Some(message) = &self.error_message {
            ui.colored_label(egui::Color32::from_rgb(248, 113, 113), message);
        }
        if let Some(message) = &self.save_message {
            ui.colored_label(egui::Color32::from_rgb(34, 197, 94), message);
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Save").clicked() {
                self.save();
            }
        });
    }
}

fn position_label(position: PeerPosition) -> &'static str {
    match position {
        PeerPosition::Left => "Left",
        PeerPosition::Right => "Right",
        PeerPosition::Top => "Top",
        PeerPosition::Bottom => "Bottom",
    }
}

fn game_compatibility_label(mode: GameCompatibilityMode) -> &'static str {
    match mode {
        GameCompatibilityMode::AlwaysEnabled => "Always enabled",
        GameCompatibilityMode::Borderless => "Borderless games",
        GameCompatibilityMode::Compatible => "Safe / compatible",
    }
}

fn pairing_text(pairing: &PairingUiState) -> String {
    match pairing {
        PairingUiState::Idle => "No pending pairing".to_string(),
        PairingUiState::PendingIncoming(pairing) => {
            format!("Incoming request from {}", pairing.peer_name)
        }
        PairingUiState::PendingOutgoing(pairing) => {
            format!("Waiting for {}", pairing.peer_name)
        }
        PairingUiState::Paired {
            peer_name,
            peer_fingerprint,
        } => {
            format!("Paired with {peer_name} ({peer_fingerprint})")
        }
        PairingUiState::Error(error) => error.clone(),
    }
}
