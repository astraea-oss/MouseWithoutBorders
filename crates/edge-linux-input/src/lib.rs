use std::process::Stdio;

use edge_common::ClipboardConfig;
use edge_protocol::{InputEvent, OutputInfo, ScreenInfo};
use serde::Deserialize;
use tokio::{io::AsyncWriteExt, process::Command};

pub const LIBEI_PKG_CONFIG: &str = "libei-1.0";

#[derive(Debug, thiserror::Error)]
pub enum LinuxInputError {
    #[error("{pkg_config} is not available through pkg-config")]
    LibeiUnavailable { pkg_config: &'static str },
    #[error("{pkg_config} is installed, but libei input injection is not implemented yet")]
    LibeiInjectionNotImplemented { pkg_config: &'static str },
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
    version: Option<String>,
}

impl LibeiBackend {
    pub fn probe() -> Self {
        let version = std::process::Command::new("pkg-config")
            .arg("--modversion")
            .arg(LIBEI_PKG_CONFIG)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|version| !version.is_empty());

        Self {
            available: version.is_some(),
            version,
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn pkg_config_name(&self) -> &'static str {
        LIBEI_PKG_CONFIG
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub async fn inject(&self, event: InputEvent) -> Result<()> {
        if !self.available {
            return Err(LinuxInputError::LibeiUnavailable {
                pkg_config: LIBEI_PKG_CONFIG,
            });
        }

        tracing::debug!(?event, "libei injection requested");
        Err(LinuxInputError::LibeiInjectionNotImplemented {
            pkg_config: LIBEI_PKG_CONFIG,
        })
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
