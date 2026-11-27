//! The only operations the bundled pages can ask for. Each maps onto one
//! backend request, or onto a fixed local action (open the log, start the
//! backend). Nothing takes a path, key or command line from JavaScript
//! except the Quickshell shell file the user picks, which the backend
//! validates before touching it.

use std::{path::PathBuf, sync::Arc};

use edge_common::AppConfig;
use edge_control::{
    SettingsDraft,
    protocol::{
        AudioTarget, Controller, ErrorKind, PhonePairingOffer, Reply, Request, SettingsView,
    },
};
use serde::Serialize;
use tauri::State;

use crate::backend::{Backend, UiError, UiState};

type Result<T> = std::result::Result<T, UiError>;

fn unexpected(reply: Reply) -> UiError {
    UiError::new(
        ErrorKind::Failed,
        format!("unexpected reply from the backend: {reply:?}"),
    )
}

async fn done(backend: &Backend, request: Request) -> Result<()> {
    match backend.request(request).await? {
        Reply::Done => Ok(()),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub fn ui_state(backend: State<'_, Arc<Backend>>) -> UiState {
    backend.state()
}

#[tauri::command]
pub async fn set_input_forwarding(backend: State<'_, Arc<Backend>>, enabled: bool) -> Result<()> {
    done(&backend, Request::SetInputForwarding { enabled }).await
}

#[tauri::command]
pub async fn set_controller(
    backend: State<'_, Arc<Backend>>,
    controller: Controller,
) -> Result<()> {
    done(&backend, Request::SetController { controller }).await
}

#[tauri::command]
pub async fn set_audio(backend: State<'_, Arc<Backend>>, target: AudioTarget) -> Result<()> {
    done(&backend, Request::SetAudio { target }).await
}

#[tauri::command]
pub async fn disconnect_peer(backend: State<'_, Arc<Backend>>) -> Result<()> {
    done(&backend, Request::Disconnect).await
}

#[tauri::command]
pub async fn reconnect_peer(backend: State<'_, Arc<Backend>>) -> Result<()> {
    done(&backend, Request::Reconnect).await
}

#[tauri::command]
pub async fn arm_computer_pairing(backend: State<'_, Arc<Backend>>) -> Result<()> {
    done(&backend, Request::ArmComputerPairing).await
}

#[derive(Debug, Serialize)]
pub struct PairingCode {
    pub uri: String,
    pub expires_in_secs: u64,
    /// The link as an SVG QR code, drawn here so the page needs no library.
    pub qr_svg: String,
}

#[tauri::command]
pub async fn start_phone_pairing(backend: State<'_, Arc<Backend>>) -> Result<PairingCode> {
    match backend.request(Request::StartPhonePairing).await? {
        Reply::PhonePairing(PhonePairingOffer {
            uri,
            expires_in_secs,
        }) => Ok(PairingCode {
            qr_svg: qr_svg(&uri)?,
            uri,
            expires_in_secs,
        }),
        other => Err(unexpected(other)),
    }
}

fn qr_svg(data: &str) -> Result<String> {
    use qrcode::{QrCode, render::svg};
    let code = QrCode::new(data.as_bytes())
        .map_err(|e| UiError::new(ErrorKind::Failed, format!("pairing link too long: {e}")))?;
    Ok(code
        .render::<svg::Color>()
        .quiet_zone(false)
        .dark_color(svg::Color("#111111"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[tauri::command]
pub async fn cancel_phone_pairing(backend: State<'_, Arc<Backend>>) -> Result<()> {
    done(&backend, Request::CancelPhonePairing).await
}

#[tauri::command]
pub async fn unpair_phone(backend: State<'_, Arc<Backend>>, name: String) -> Result<()> {
    done(&backend, Request::UnpairPhone { name }).await
}

/// The persisted settings. While the backend is down this reads the file
/// it last used, so settings stay editable.
#[tauri::command]
pub async fn load_settings(backend: State<'_, Arc<Backend>>) -> Result<SettingsView> {
    if backend.is_connected() {
        return match backend.request(Request::Settings).await? {
            Reply::Settings(view) => Ok(*view),
            other => Err(unexpected(other)),
        };
    }
    let path = offline_path(&backend)?;
    let config = load_config(&path).await?;
    Ok(SettingsView {
        draft: SettingsDraft::from_config(&config),
        config_path: path,
        connector_only: cfg!(windows),
        local_address: edge_common::local_ip_toward(&config.peer.host).map(|ip| ip.to_string()),
        audio_buffers_ms: edge_control::AUDIO_BUFFERS_MS.to_vec(),
    })
}

fn offline_path(backend: &Backend) -> Result<PathBuf> {
    backend.offline_config().ok_or_else(|| {
        UiError::new(
            ErrorKind::Unavailable,
            "start the backend once so hyprKvm knows which settings file it uses",
        )
    })
}

async fn load_config(path: &std::path::Path) -> Result<AppConfig> {
    AppConfig::load(path).await.map_err(|e| {
        UiError::new(
            ErrorKind::Failed,
            format!("couldn't read {}: {e}", path.display()),
        )
    })
}

#[tauri::command]
pub async fn validate_settings(
    backend: State<'_, Arc<Backend>>,
    draft: SettingsDraft,
) -> Result<Vec<edge_control::FieldError>> {
    if backend.is_connected() {
        return match backend.request(Request::ValidateSettings { draft }).await? {
            Reply::Validation { errors } => Ok(errors),
            other => Err(unexpected(other)),
        };
    }
    let config = load_config(&offline_path(&backend)?).await?;
    Ok(draft.validate(cfg!(windows), &config.notifications, None))
}

#[derive(Debug, Serialize)]
pub struct SaveOutcome {
    /// The backend is restarting to apply them.
    pub restarting: bool,
}

#[tauri::command]
pub async fn save_settings(
    backend: State<'_, Arc<Backend>>,
    draft: SettingsDraft,
    base: SettingsDraft,
) -> Result<SaveOutcome> {
    if backend.is_connected() {
        return match backend
            .request(Request::SaveSettings { draft, base })
            .await?
        {
            Reply::Saved { restarting } => Ok(SaveOutcome { restarting }),
            other => Err(unexpected(other)),
        };
    }
    // Nothing is running, so the next start picks these up.
    let path = offline_path(&backend)?;
    let config = load_config(&path).await?;
    if SettingsDraft::from_config(&config) != base {
        return Err(UiError::new(
            ErrorKind::Conflict,
            "the settings file changed since this page loaded; reload and try again",
        ));
    }
    let errors = draft.validate(cfg!(windows), &config.notifications, None);
    if !errors.is_empty() {
        return Err(edge_control::protocol::ControlError::invalid(errors).into());
    }
    let updated = draft
        .apply(&config, cfg!(windows))
        .map_err(|e| UiError::new(ErrorKind::Invalid, format!("{e:#}")))?;
    updated
        .save(&path)
        .await
        .map_err(|e| UiError::new(ErrorKind::Failed, format!("couldn't save: {e}")))?;
    Ok(SaveOutcome { restarting: false })
}

#[tauri::command]
pub async fn get_autostart(backend: State<'_, Arc<Backend>>) -> Result<Option<bool>> {
    match backend.request(Request::Autostart).await? {
        Reply::Autostart { enabled } => Ok(enabled),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn set_autostart(backend: State<'_, Arc<Backend>>, enabled: bool) -> Result<()> {
    done(&backend, Request::SetAutostart { enabled }).await
}

#[tauri::command]
pub async fn get_notifications(
    backend: State<'_, Arc<Backend>>,
) -> Result<Option<edge_control::NotificationSetup>> {
    match backend.request(Request::Notifications).await? {
        Reply::Notifications { setup } => Ok(setup.map(|setup| *setup)),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn install_notification_bridge(
    backend: State<'_, Arc<Backend>>,
    shell_path: PathBuf,
) -> Result<String> {
    match backend
        .request(Request::InstallNotificationBridge { shell_path })
        .await?
    {
        Reply::Message { text } => Ok(text),
        other => Err(unexpected(other)),
    }
}

/// Sends this computer's clipboard to a phone; returns what happened.
#[tauri::command]
pub async fn send_clipboard_to_phone(
    backend: State<'_, Arc<Backend>>,
    fingerprint: String,
) -> Result<String> {
    match backend
        .request(Request::SendClipboardToPhone { fingerprint })
        .await?
    {
        Reply::Message { text } => Ok(text),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn stop_phone_view(backend: State<'_, Arc<Backend>>, fingerprint: String) -> Result<()> {
    done(&backend, Request::StopPhoneView { fingerprint }).await
}

#[tauri::command]
pub async fn diagnostics(backend: State<'_, Arc<Backend>>) -> Result<String> {
    let ui = format!(
        "hyprKvm UI {} ({})\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    match backend.request(Request::Diagnostics).await {
        Ok(Reply::Diagnostics { report }) => Ok(ui + &report),
        Ok(other) => Err(unexpected(other)),
        Err(error) => Ok(format!("{ui}backend: {}\n", error.message)),
    }
}

#[tauri::command]
pub async fn quit_backend(backend: State<'_, Arc<Backend>>) -> Result<()> {
    done(&backend, Request::Quit).await
}

/// Starts the backend: through the user service when one is installed, so
/// there's never a second copy beside it, otherwise the executable that sits
/// next to this one.
#[tauri::command]
pub async fn start_backend(backend: State<'_, Arc<Backend>>) -> Result<String> {
    if backend.is_connected() {
        return Ok("already running".into());
    }
    let how = crate::launch::start_backend()
        .await
        .map_err(|e| UiError::new(ErrorKind::Failed, format!("{e:#}")))?;
    backend.mark_starting();
    Ok(how)
}

#[tauri::command]
pub async fn open_log(backend: State<'_, Arc<Backend>>) -> Result<()> {
    let hello = backend
        .hello()
        .ok_or_else(|| UiError::new(ErrorKind::Unavailable, "the backend isn't connected"))?;
    crate::launch::open_path(&hello.log_path)
        .map_err(|e| UiError::new(ErrorKind::Failed, format!("{e:#}")))
}

#[tauri::command]
pub fn open_phone_pairing(app: tauri::AppHandle) -> Result<()> {
    crate::windows::open_pair_phone(&app)
        .map_err(|e| UiError::new(ErrorKind::Failed, format!("{e:#}")))
}
