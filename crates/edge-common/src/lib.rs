use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    #[error("failed to read {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to write {path}: {source}")]
    WriteConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to encode config: {0}")]
    EncodeConfig(toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, CommonError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Controller,
    Receiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerPosition {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub role: Role,
    #[serde(default)]
    pub release_hotkey: Option<String>,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub monitor: Option<String>,
    #[serde(default)]
    pub peer: PeerConfigSection,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerConfigSection {
    #[serde(default)]
    pub laptop: Option<PeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub host: String,
    pub port: u16,
    pub position: PeerPosition,
    #[serde(default)]
    pub pinned_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    pub backend: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            backend: "libei".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub text_only: bool,
    pub max_bytes: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            text_only: true,
            max_bytes: 1_048_576,
        }
    }
}

impl AppConfig {
    pub fn controller_default() -> Self {
        Self {
            device_name: "Main PC".to_string(),
            role: Role::Controller,
            release_hotkey: Some("Ctrl+Alt+Pause".to_string()),
            listen: None,
            monitor: None,
            peer: PeerConfigSection {
                laptop: Some(PeerConfig {
                    host: "192.168.0.11".to_string(),
                    port: 42_420,
                    position: PeerPosition::Left,
                    pinned_fingerprint: String::new(),
                }),
            },
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
        }
    }

    pub fn receiver_default() -> Self {
        Self {
            device_name: "Lua".to_string(),
            role: Role::Receiver,
            release_hotkey: None,
            listen: Some("0.0.0.0:42420".to_string()),
            monitor: Some("eDP-1".to_string()),
            peer: PeerConfigSection::default(),
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
        }
    }

    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|source| CommonError::ReadConfig {
                    path: path.to_path_buf(),
                    source,
                })?;
        toml::from_str(&text).map_err(|source| CommonError::ParseConfig {
            path: path.to_path_buf(),
            source,
        })
    }

    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| CommonError::WriteConfig {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let text = toml::to_string_pretty(self).map_err(CommonError::EncodeConfig)?;
        tokio::fs::write(path, text)
            .await
            .map_err(|source| CommonError::WriteConfig {
                path: path.to_path_buf(),
                source,
            })
    }
}

pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "edge_kvm=info,info".into());

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

pub fn default_state_dir() -> PathBuf {
    std::env::var_os("EDGE_KVM_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("edge-kvm"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".local/state/edge-kvm"))
        })
        .unwrap_or_else(|| PathBuf::from(".edge-kvm-state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receiver.toml");
        let expected = AppConfig::receiver_default();

        expected.save(&path).await.unwrap();
        let actual = AppConfig::load(&path).await.unwrap();

        assert_eq!(actual.role, Role::Receiver);
        assert_eq!(actual.listen.as_deref(), Some("0.0.0.0:42420"));
        assert_eq!(actual.clipboard.max_bytes, 1_048_576);
    }
}
