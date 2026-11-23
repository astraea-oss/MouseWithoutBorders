//! hyprKvm: the desktop UI for an edge-kvm backend.
//!
//! The backend owns sessions, identity, pins and settings, and keeps running
//! when this closes or crashes. This process only shows what the backend
//! reports and asks it to act.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod commands;
mod launch;
mod windows;

use std::path::PathBuf;

use tauri::{Manager, WindowEvent};

use crate::windows::Route;

/// `--config <path>`, or `EDGE_KVM_CONFIG`: attach only to the backend that
/// uses this settings file.
fn expected_config(args: &[String]) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(path) = arg.strip_prefix("--config=") {
            return Some(PathBuf::from(path));
        }
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("EDGE_KVM_CONFIG").map(PathBuf::from)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let first_route = Route::from_args(&args);
    let expected = expected_config(&args);

    tauri::Builder::default()
        // A second launch (tray, desktop entry) routes this instance instead.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Err(error) = windows::route(app, Route::from_args(&argv)) {
                eprintln!("hyprKvm: couldn't open the requested window: {error:#}");
            }
        }))
        .invoke_handler(tauri::generate_handler![
            commands::ui_state,
            commands::set_input_forwarding,
            commands::set_controller,
            commands::set_audio,
            commands::disconnect_peer,
            commands::reconnect_peer,
            commands::arm_computer_pairing,
            commands::start_phone_pairing,
            commands::cancel_phone_pairing,
            commands::unpair_phone,
            commands::load_settings,
            commands::validate_settings,
            commands::save_settings,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_notifications,
            commands::install_notification_bridge,
            commands::diagnostics,
            commands::quit_backend,
            commands::start_backend,
            commands::open_log,
            commands::open_phone_pairing,
        ])
        .setup(move |app| {
            let backend = backend::Backend::new(app.handle().clone(), expected);
            app.manage(backend.clone());
            tauri::async_runtime::spawn(backend.run());
            windows::route(app.handle(), first_route)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the popup withdraws its code; it never approves anything.
            if window.label() == windows::PAIR_PHONE && matches!(event, WindowEvent::Destroyed) {
                let backend = window.state::<std::sync::Arc<backend::Backend>>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = backend
                        .request(edge_control::protocol::Request::CancelPhonePairing)
                        .await;
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("hyprKvm failed to start");
}
