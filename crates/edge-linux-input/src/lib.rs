use std::process::Stdio;

use edge_common::ClipboardConfig;
use edge_protocol::{InputEvent, OutputInfo, ScreenInfo};
use serde::Deserialize;
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Debug, thiserror::Error)]
pub enum LinuxInputError {
    #[error("libei is not available through pkg-config")]
    LibeiUnavailable,
    #[error("command `{program}` failed: {message}")]
    CommandFailed { program: String, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("clipboard text exceeds configured max_bytes ({max_bytes})")]
    ClipboardTooLarge { max_bytes: usize },
}

pub type Result<T> = std::result::Result<T, LinuxInputError>;

#[derive(Debug, Clone)]
pub struct LibeiBackend {
    available: bool,
}

impl LibeiBackend {
    pub fn probe() -> Self {
        let available = std::process::Command::new("pkg-config")
            .arg("--exists")
            .arg("libei")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        Self { available }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub async fn inject(&self, event: InputEvent) -> Result<()> {
        if !self.available {
            return Err(LinuxInputError::LibeiUnavailable);
        }

        // The FFI backend is intentionally isolated behind this method. Until generated
        // bindings are wired in, tests and the receiver can exercise the full command path
        // while failing closed when the compositor backend is absent.
        tracing::info!(?event, "libei input injection placeholder");
        Ok(())
    }

    pub async fn all_keys_up(&self) -> Result<()> {
        self.inject(InputEvent::AllKeysUp).await
    }
}

pub async fn hyprland_screen_info(primary: &str) -> Result<ScreenInfo> {
    let output = Command::new("hyprctl")
        .arg("monitors")
        .arg("-j")
        .output()
        .await?;
    if !output.status.success() {
        return Err(LinuxInputError::CommandFailed {
            program: "hyprctl".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let monitors: Vec<HyprMonitor> = serde_json::from_slice(&output.stdout)?;
    let outputs = monitors
        .into_iter()
        .map(|monitor| OutputInfo {
            name: monitor.name,
            width: monitor.width,
            height: monitor.height,
            scale: monitor.scale,
            x: monitor.x,
            y: monitor.y,
        })
        .collect();

    Ok(ScreenInfo {
        outputs,
        primary_output: primary.to_string(),
    })
}

pub async fn read_clipboard_text(config: &ClipboardConfig) -> Result<Option<String>> {
    if !config.enabled {
        return Ok(None);
    }

    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .arg("--type")
        .arg("text")
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    if output.stdout.len() > config.max_bytes {
        return Err(LinuxInputError::ClipboardTooLarge {
            max_bytes: config.max_bytes,
        });
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

pub async fn write_clipboard_text(config: &ClipboardConfig, text: &str) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    if text.len() > config.max_bytes {
        return Err(LinuxInputError::ClipboardTooLarge {
            max_bytes: config.max_bytes,
        });
    }

    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = &mut child.stdin {
        stdin.write_all(text.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(LinuxInputError::CommandFailed {
            program: "wl-copy".to_string(),
            message: format!("exited with {status}"),
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HyprMonitor {
    name: String,
    width: u32,
    height: u32,
    #[serde(default)]
    scale: f32,
    x: i32,
    y: i32,
}
