use edge_keymap::{WindowsScanCode, windows_scancode_to_evdev};

#[derive(Debug, thiserror::Error)]
pub enum WindowsInputError {
    #[error("Windows input capture is only available on Windows")]
    UnsupportedPlatform,
    #[error("unmapped Windows scan code {scan_code:#x}, extended={extended}")]
    UnmappedKey { scan_code: u16, extended: bool },
}

pub type Result<T> = std::result::Result<T, WindowsInputError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    LocalActive,
    RemoteActive,
}

pub fn map_key(scan_code: u16, extended: bool) -> Result<u16> {
    windows_scancode_to_evdev(WindowsScanCode {
        scan_code,
        extended,
    })
    .ok_or(WindowsInputError::UnmappedKey {
        scan_code,
        extended,
    })
}

#[cfg(windows)]
pub fn install_hooks() -> Result<()> {
    tracing::info!("Windows hook installation placeholder");
    Ok(())
}

#[cfg(not(windows))]
pub fn install_hooks() -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}
