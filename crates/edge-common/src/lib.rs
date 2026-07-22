use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
};

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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    #[error("device name must not be empty")]
    EmptyDeviceName,
    #[error("port must be between 1 and 65535")]
    InvalidPort,
    #[error("host must not be empty")]
    EmptyHost,
    #[error("listen address must include a port")]
    MissingListenPort,
    #[error("listen port is invalid")]
    InvalidListenPort,
}

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
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioLocalPlayback {
    Redirect,
    Mirror,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub enabled: bool,
    pub local_playback: AudioLocalPlayback,
    pub jitter_target_ms: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            local_playback: AudioLocalPlayback::Redirect,
            jitter_target_ms: 60,
        }
    }
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
    #[serde(default)]
    pub game_compatibility: GameCompatibilityMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GameCompatibilityMode {
    Compatible,
    Borderless,
    #[default]
    AlwaysEnabled,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
            game_compatibility: GameCompatibilityMode::default(),
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
            audio: AudioConfig {
                enabled: true,
                ..AudioConfig::default()
            },
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
            audio: AudioConfig::default(),
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

    pub fn load_blocking(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| CommonError::ReadConfig {
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

    pub fn save_blocking(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CommonError::WriteConfig {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self).map_err(CommonError::EncodeConfig)?;
        std::fs::write(path, text).map_err(|source| CommonError::WriteConfig {
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
        .unwrap_or_else(|| portable_app_dir().join("state"))
}

pub fn portable_config_path(file_name: &str) -> PathBuf {
    std::env::var_os("EDGE_KVM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| portable_app_dir().join(file_name))
}

pub fn portable_app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn detect_primary_local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
    socket.connect(SocketAddr::from(([1, 1, 1, 1], 80))).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

pub fn parse_listen_port(listen: &str) -> std::result::Result<u16, ConfigValidationError> {
    let (_, port) = split_host_port(listen).ok_or(ConfigValidationError::MissingListenPort)?;
    port.parse()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ConfigValidationError::InvalidListenPort)
}

pub fn update_listen_port(listen: Option<&str>, port: u16) -> String {
    let host = listen
        .and_then(split_host_port)
        .map(|(host, _)| host)
        .filter(|host| !host.trim().is_empty())
        .unwrap_or("0.0.0.0");

    if host.starts_with('[') && host.ends_with(']') {
        format!("{host}:{port}")
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn validate_device_name(name: &str) -> std::result::Result<(), ConfigValidationError> {
    if name.trim().is_empty() {
        Err(ConfigValidationError::EmptyDeviceName)
    } else {
        Ok(())
    }
}

pub fn validate_port(port: u16) -> std::result::Result<(), ConfigValidationError> {
    if port == 0 {
        Err(ConfigValidationError::InvalidPort)
    } else {
        Ok(())
    }
}

pub fn validate_host(host: &str) -> std::result::Result<(), ConfigValidationError> {
    if host.trim().is_empty() {
        Err(ConfigValidationError::EmptyHost)
    } else {
        Ok(())
    }
}

fn split_host_port(value: &str) -> Option<(&str, &str)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        Some((host, port))
    } else {
        value.rsplit_once(':')
    }
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
        assert!(!actual.audio.enabled);
        assert_eq!(actual.audio.local_playback, AudioLocalPlayback::Redirect);
    }

    #[test]
    fn new_controller_configs_enable_audio() {
        assert!(AppConfig::controller_default().audio.enabled);
    }

    #[test]
    fn legacy_config_defaults_audio_to_disabled() {
        let config: AppConfig = toml::from_str(
            r#"
device_name = "Legacy"
role = "receiver"
listen = "0.0.0.0:42420"
"#,
        )
        .unwrap();
        assert!(!config.audio.enabled);
        assert_eq!(config.audio.jitter_target_ms, 60);
    }

    #[test]
    fn legacy_input_config_defaults_to_always_enabled_for_games() {
        let input: InputConfig = toml::from_str("backend = \"auto\"").unwrap();
        assert_eq!(
            input.game_compatibility,
            GameCompatibilityMode::AlwaysEnabled
        );
    }

    #[test]
    fn update_listen_port_preserves_host() {
        assert_eq!(
            update_listen_port(Some("127.0.0.1:42420"), 42421),
            "127.0.0.1:42421"
        );
        assert_eq!(update_listen_port(None, 42420), "0.0.0.0:42420");
    }

    #[test]
    fn parse_listen_port_rejects_missing_or_zero_port() {
        assert_eq!(parse_listen_port("0.0.0.0:42420").unwrap(), 42420);
        assert_eq!(
            parse_listen_port("0.0.0.0"),
            Err(ConfigValidationError::MissingListenPort)
        );
        assert_eq!(
            parse_listen_port("0.0.0.0:0"),
            Err(ConfigValidationError::InvalidListenPort)
        );
    }

    #[test]
    fn validation_rejects_empty_name_and_host() {
        assert_eq!(
            validate_device_name("  "),
            Err(ConfigValidationError::EmptyDeviceName)
        );
        assert_eq!(validate_host(""), Err(ConfigValidationError::EmptyHost));
        assert_eq!(validate_port(0), Err(ConfigValidationError::InvalidPort));
    }
}
