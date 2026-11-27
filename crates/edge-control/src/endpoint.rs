//! Where a backend serves local control.
//!
//! Linux: `$XDG_RUNTIME_DIR/edge-kvm/control.sock`, mode 0600 in a 0700
//! directory, so only the current user can connect. This is the socket the
//! text commands have always used.
//!
//! Windows: a named pipe whose name includes the user, created with an ACL
//! that admits only the current user. (Served by the Windows backend once it
//! gains a control endpoint.)

#[cfg(unix)]
use std::path::PathBuf;

/// The control socket, or `None` without a runtime directory.
#[cfg(unix)]
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("edge-kvm").join("control.sock"))
}

/// The control pipe for the current user.
#[cfg(windows)]
pub fn pipe_name() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!(r"\\.\pipe\edge-kvm-control-{user}")
}

/// The endpoint as a display string, for messages.
pub fn describe() -> String {
    #[cfg(unix)]
    {
        socket_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(no XDG_RUNTIME_DIR)".to_string())
    }
    #[cfg(windows)]
    {
        pipe_name()
    }
}
