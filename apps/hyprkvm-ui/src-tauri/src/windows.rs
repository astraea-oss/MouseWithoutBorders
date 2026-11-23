//! The main window (Home and View) and the pairing popups.
//!
//! On Linux the windows have no decorations: Hyprland's border is the only
//! frame. GTK gives the whole app one Wayland app ID, so popups can't have a
//! class of their own; Hyprland rules match them by title instead (see
//! `apps/hyprkvm-ui/README.md`). Windows keeps native frames so every window
//! can still be moved, resized and closed.

use anyhow::Result;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const MAIN: &str = "main";
pub const PAIR_PHONE: &str = "pair-phone";
pub const PAIR_PHONE_TITLE: &str = "Add a phone · hyprKvm";

const DECORATED: bool = cfg!(windows);

/// Where a launch or second launch asked to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Home,
    View,
    PairPhone,
}

impl Route {
    /// `--route home|view|pair-phone`; Home without one.
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Self {
        let mut args = args.iter().map(AsRef::as_ref);
        while let Some(arg) = args.next() {
            let value = match arg.strip_prefix("--route=") {
                Some(value) => Some(value),
                None if arg == "--route" => args.next(),
                None => None,
            };
            match value {
                Some("view") => return Self::View,
                Some("pair-phone") => return Self::PairPhone,
                Some(_) => return Self::Home,
                None => {}
            }
        }
        Self::Home
    }
}

pub fn route(app: &AppHandle, route: Route) -> Result<()> {
    match route {
        Route::Home | Route::View => {
            let window = open_main(app)?;
            let path = if route == Route::View { "/view" } else { "/" };
            window.emit("ui://navigate", path)?;
            window.unminimize().ok();
            window.show().ok();
            window.set_focus().ok();
        }
        Route::PairPhone => open_pair_phone(app)?,
    }
    Ok(())
}

fn open_main(app: &AppHandle) -> Result<tauri::WebviewWindow> {
    if let Some(window) = app.get_webview_window(MAIN) {
        return Ok(window);
    }
    Ok(
        WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
            .title("hyprKvm")
            .inner_size(874.0, 900.0)
            .min_inner_size(279.0, 420.0)
            .decorations(DECORATED)
            .build()?,
    )
}

/// The Add a phone popup: one at a time, focused if already open.
pub fn open_pair_phone(app: &AppHandle) -> Result<()> {
    if let Some(window) = app.get_webview_window(PAIR_PHONE) {
        window.unminimize().ok();
        window.set_focus().ok();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, PAIR_PHONE, WebviewUrl::App("pair-phone".into()))
        .title(PAIR_PHONE_TITLE)
        .inner_size(380.0, 520.0)
        .resizable(false)
        .decorations(DECORATED)
        .center()
        .build()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Route;

    #[test]
    fn routes_come_from_either_flag_form() {
        assert_eq!(Route::from_args(&["hyprkvm"]), Route::Home);
        assert_eq!(Route::from_args(&["hyprkvm", "--route", "view"]), Route::View);
        assert_eq!(
            Route::from_args(&["hyprkvm", "--route=pair-phone"]),
            Route::PairPhone
        );
        assert_eq!(Route::from_args(&["hyprkvm", "--route", "nope"]), Route::Home);
    }
}
