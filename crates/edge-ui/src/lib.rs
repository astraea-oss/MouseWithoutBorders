//! The edge-kvm windows: settings, phone pairing and pairing confirmation.

mod phone;
mod settings;
mod widgets;

use std::{
    fs::OpenOptions,
    io::Write,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use edge_common::{AppConfig, Role};

use crate::{
    phone::{PairingAction, PhonePairing},
    settings::SettingsApp,
};

static SETTINGS_WINDOW_OPEN: OnceLock<Mutex<bool>> = OnceLock::new();
static SETTINGS_CONTEXT: OnceLock<Mutex<Option<eframe::egui::Context>>> = OnceLock::new();

pub struct SettingsUiInput {
    pub role: Role,
    /// The Windows app can only dial, so it needs the other computer's
    /// address. A Linux node also accepts computers that dial it.
    pub connector_only: bool,
    /// Close the settings window after a successful save so the caller can
    /// restart the node with the new connection configuration.
    pub restart_on_save: bool,
    pub config_path: PathBuf,
    pub config: AppConfig,
    pub local_ip: Option<IpAddr>,
    pub pairing: PairingUiState,
    /// The running node, for live status, phones and pairing. `None` hides
    /// what needs it.
    pub link: Option<Arc<dyn NodeLink>>,
}

/// What the windows can ask of the running node. The Linux receiver answers
/// over its control socket. Calls block briefly, so implementations should
/// time out quickly.
pub trait NodeLink: Send + Sync {
    /// Linux desktop sharing capabilities and current health. Older nodes return None.
    fn notifications(&self) -> Option<NotificationSetup> {
        None
    }
    fn install_notification_bridge(&self, _shell: &std::path::Path) -> Result<String> {
        anyhow::bail!("Quickshell integration is unavailable on this platform")
    }
    /// Live status, or `None` if the node can't be reached.
    fn status(&self) -> Option<NodeStatus>;
    /// Paired phones, or `None` where phones aren't supported.
    fn phones(&self) -> Option<Vec<PhoneEntry>>;
    fn unpair_phone(&self, name: &str) -> Result<()>;
    /// Arms two-sided pairing with another computer.
    fn arm_pairing(&self) -> Result<()>;
    /// Opens a phone pairing window. Events arrive until it closes.
    fn pair_phone(&self) -> Result<mpsc::Receiver<PhonePairingEvent>>;
    /// Whether the node starts at login, or `None` if that can't be set here.
    fn autostart(&self) -> Option<bool>;
    fn set_autostart(&self, enabled: bool) -> Result<()>;
    fn open_log(&self) -> Result<()>;
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NotificationSetup {
    pub identity: String,
    pub computers: Vec<NotificationComputer>,
    pub status: String,
    pub active: bool,
    pub has_store: bool,
    pub coordinator: String,
    pub shell_path: Option<PathBuf>,
    pub socket_path: PathBuf,
    pub bridge_installed: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NotificationComputer {
    pub name: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeStatus {
    pub summary: String,
    pub connected: bool,
    pub peer: Option<String>,
    pub round_trip_ms: Option<u64>,
    pub diagnostics: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneEntry {
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhonePairingEvent {
    /// Show this link as a QR code until it expires.
    Offer {
        uri: String,
        expires_in: Duration,
    },
    Paired(String),
    /// Expired or replaced by a newer pairing.
    Closed,
    Failed(String),
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
        let window_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_settings_window(input)));
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
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([480.0, 660.0])
            .with_min_inner_size([420.0, 420.0]),
        ..Default::default()
    };
    configure_event_loop(&mut options);

    eframe::run_native(
        "edge-kvm settings",
        options,
        Box::new(move |cc| {
            widgets::apply_style(&cc.egui_ctx);
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

/// A small window that only pairs a phone, for the tray's "Add phone…".
pub fn run_phone_pairing_window(link: Arc<dyn NodeLink>) -> Result<()> {
    let mut options = eframe::NativeOptions {
        persist_window: false,
        persistence_path: None,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([360.0, 470.0])
            .with_resizable(false),
        ..Default::default()
    };
    configure_event_loop(&mut options);

    eframe::run_native(
        "Add a phone",
        options,
        Box::new(move |cc| {
            widgets::apply_style(&cc.egui_ctx);
            let pairing = PhonePairing::start(link.as_ref());
            Ok(Box::new(PhonePairingApp { link, pairing }))
        }),
    )
    .map_err(|err| anyhow::anyhow!("failed to run phone pairing window: {err}"))
}

struct PhonePairingApp {
    link: Arc<dyn NodeLink>,
    pairing: PhonePairing,
}

impl eframe::App for PhonePairingApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::new()
                    .fill(widgets::WINDOW)
                    .inner_margin(eframe::egui::Margin::same(18)),
            )
            .show(ui, |ui| {
                if self.pairing.ui(ui, self.link.as_ref()) == PairingAction::Close {
                    ui.ctx()
                        .send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                }
            });
    }
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
