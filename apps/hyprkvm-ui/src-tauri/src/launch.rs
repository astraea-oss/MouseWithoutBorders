//! Starting the backend and opening files, without a shell.

use std::{path::Path, process::Command};

use anyhow::{Context, Result};

/// The user unit installed by `scripts/install-linux-receiver.sh`.
#[cfg(target_os = "linux")]
const SERVICE: &str = "edge-kvm-receiver.service";

#[cfg(target_os = "linux")]
const BACKEND: &str = "edge-receiver-linux";
#[cfg(windows)]
const BACKEND: &str = "edge-controller-win.exe";

/// Starts the platform backend and says how.
pub async fn start_backend() -> Result<String> {
    tokio::task::spawn_blocking(start_blocking).await?
}

fn start_blocking() -> Result<String> {
    #[cfg(target_os = "linux")]
    if service_installed() {
        let status = Command::new("systemctl")
            .args(["--user", "start", SERVICE])
            .status()
            .context("failed to run systemctl")?;
        anyhow::ensure!(status.success(), "systemctl couldn't start {SERVICE}");
        return Ok(format!("started {SERVICE}"));
    }

    let executable = sibling_backend().context(format!(
        "couldn't find {BACKEND} next to hyprKvm; start it yourself or reinstall both together"
    ))?;
    let mut command = Command::new(&executable);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = executable.parent() {
        command.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so closing the UI's terminal doesn't stop it.
        command.process_group(0);
    }
    command
        .spawn()
        .with_context(|| format!("failed to start {}", executable.display()))?;
    Ok(format!("started {}", executable.display()))
}

#[cfg(target_os = "linux")]
fn service_installed() -> bool {
    Command::new("systemctl")
        .args(["--user", "cat", SERVICE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The backend ships beside the UI, in portable and installed layouts alike.
fn sibling_backend() -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join(BACKEND);
    candidate.is_file().then_some(candidate)
}

/// Opens a file with the desktop's default app.
pub fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(path);
        command
    };
    command
        .spawn()
        .with_context(|| format!("couldn't open {}", path.display()))?;
    Ok(())
}
