use std::{
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use anyhow::{Context, Result};
use edge_common::{
    AppConfig, PeerConfig, PeerPosition, Role, parse_listen_port, update_listen_port,
    validate_device_name, validate_host, validate_port,
};

static SETTINGS_WINDOW_OPEN: OnceLock<Mutex<bool>> = OnceLock::new();

pub struct SettingsUiInput {
    pub role: Role,
    pub config_path: PathBuf,
    pub config: AppConfig,
    pub local_ip: Option<IpAddr>,
    pub pairing: PairingUiState,
}

#[derive(Debug, Clone)]
pub enum SettingsUiResult {
    Saved(AppConfig),
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
            return;
        }
        *open = true;
    }

    std::thread::spawn(move || {
        let _ = run_settings_window(input);
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
    let options = eframe::NativeOptions {
        persist_window: false,
        persistence_path: None,
        ..Default::default()
    };

    eframe::run_native(
        "edge-kvm Settings",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
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
    save_message: Option<String>,
    error_message: Option<String>,
    result: Arc<Mutex<SettingsUiResult>>,
}

impl SettingsApp {
    fn new(input: SettingsUiInput, result: Arc<Mutex<SettingsUiResult>>) -> Self {
        let peer = input.config.peer.laptop.clone();
        let port = match input.role {
            Role::Controller => peer.as_ref().map(|peer| peer.port).unwrap_or(42_420),
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
            peer_host: peer
                .as_ref()
                .map(|peer| peer.host.clone())
                .unwrap_or_default(),
            port: port.to_string(),
            position: peer
                .as_ref()
                .map(|peer| peer.position)
                .unwrap_or(PeerPosition::Left),
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
                    self.save_message = Some("Saved. Restart required.".to_string());
                    if let Ok(mut result) = self.result.lock() {
                        *result = SettingsUiResult::Saved(config);
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

        match self.role {
            Role::Controller => {
                validate_host(&self.peer_host)?;
                let peer = config.peer.laptop.get_or_insert_with(|| PeerConfig {
                    host: String::new(),
                    port,
                    position: self.position,
                    pinned_fingerprint: String::new(),
                });
                peer.host = self.peer_host.trim().to_string();
                peer.port = port;
                peer.position = self.position;
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
