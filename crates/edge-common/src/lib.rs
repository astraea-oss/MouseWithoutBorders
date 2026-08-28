use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

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

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub device_name: String,
    #[serde(default)]
    pub start_with_windows: bool,
    pub preferred_role: Role,
    pub transport: TransportMode,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub peer: PeerConfig,
    #[serde(default)]
    pub layout: LayoutConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    Connect,
    Listen,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    #[serde(default = "default_peer_name")]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub pinned_fingerprint: String,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            name: default_peer_name(),
            host: String::new(),
            port: default_port(),
            pinned_fingerprint: String::new(),
        }
    }
}

fn default_peer_name() -> String {
    "Peer".to_string()
}

const fn default_port() -> u16 {
    42_420
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub listener_position: PeerPosition,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            listener_position: PeerPosition::Left,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputConfig {
    pub capture: InputCaptureConfig,
    pub inject: InputInjectConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCaptureConfig {
    pub backend: String,
    #[serde(default)]
    pub output: String,
    #[serde(default = "default_release_hotkey")]
    pub release_hotkey: String,
    #[serde(default)]
    pub game_compatibility: GameCompatibilityMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputInjectConfig {
    pub backend: String,
    #[serde(default)]
    pub output: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GameCompatibilityMode {
    Compatible,
    Borderless,
    #[default]
    AlwaysEnabled,
}

impl Default for InputCaptureConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
            output: String::new(),
            release_hotkey: default_release_hotkey(),
            game_compatibility: GameCompatibilityMode::default(),
        }
    }
}

impl Default for InputInjectConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
            output: String::new(),
        }
    }
}

fn default_release_hotkey() -> String {
    "Ctrl+Alt+Pause".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub images_enabled: bool,
    pub max_bytes: usize,
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            images_enabled: true,
            max_bytes: 1_048_576,
            max_image_bytes: default_max_image_bytes(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_image_bytes() -> usize {
    4 * 1024 * 1024
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    device_name: String,
    #[serde(default)]
    start_with_windows: bool,
    #[serde(default)]
    preferred_role: Option<Role>,
    #[serde(default)]
    transport: Option<TransportMode>,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    release_hotkey: Option<String>,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    monitor: Option<String>,
    #[serde(default)]
    peer: ConfigPeer,
    #[serde(default)]
    layout: Option<LayoutConfig>,
    #[serde(default)]
    input: ConfigInput,
    #[serde(default)]
    clipboard: ClipboardConfig,
    #[serde(default)]
    audio: Option<AudioConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigPeer {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    pinned_fingerprint: Option<String>,
    #[serde(default)]
    laptop: Option<LegacyPeerConfig>,
}

#[derive(Debug, Deserialize)]
struct LegacyPeerConfig {
    host: String,
    port: u16,
    position: PeerPosition,
    #[serde(default)]
    pinned_fingerprint: String,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigInput {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    game_compatibility: Option<GameCompatibilityMode>,
    #[serde(default)]
    capture: Option<ConfigInputCapture>,
    #[serde(default)]
    inject: Option<ConfigInputInject>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigInputCapture {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    release_hotkey: Option<String>,
    #[serde(default)]
    game_compatibility: Option<GameCompatibilityMode>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigInputInject {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

impl ConfigFile {
    fn into_app_config(self) -> AppConfig {
        let legacy_role = self.role;
        let preferred_role = self
            .preferred_role
            .or(legacy_role)
            .or(match self.transport {
                Some(TransportMode::Connect) => Some(Role::Controller),
                Some(TransportMode::Listen) => Some(Role::Receiver),
                None => None,
            })
            .unwrap_or(Role::Receiver);
        let transport = self.transport.unwrap_or(match legacy_role {
            Some(Role::Controller) => TransportMode::Connect,
            Some(Role::Receiver) | None => TransportMode::Listen,
        });

        let ConfigPeer {
            name,
            host,
            port,
            pinned_fingerprint,
            laptop,
        } = self.peer;
        let listener_position = self
            .layout
            .map(|layout| layout.listener_position)
            .or_else(|| laptop.as_ref().map(|peer| peer.position))
            .unwrap_or(PeerPosition::Left);
        let peer = PeerConfig {
            name: name.unwrap_or_else(default_peer_name),
            host: host
                .or_else(|| laptop.as_ref().map(|peer| peer.host.clone()))
                .unwrap_or_default(),
            port: port
                .or_else(|| laptop.as_ref().map(|peer| peer.port))
                .unwrap_or_else(default_port),
            pinned_fingerprint: pinned_fingerprint
                .or_else(|| laptop.as_ref().map(|peer| peer.pinned_fingerprint.clone()))
                .unwrap_or_default(),
        };

        let legacy_backend = self.input.backend.unwrap_or_else(|| "auto".to_string());
        let capture = self.input.capture.unwrap_or_default();
        let inject = self.input.inject.unwrap_or_default();
        let input = InputConfig {
            capture: InputCaptureConfig {
                backend: capture.backend.unwrap_or_else(|| legacy_backend.clone()),
                output: capture.output.unwrap_or_default(),
                release_hotkey: capture
                    .release_hotkey
                    .or(self.release_hotkey)
                    .unwrap_or_else(default_release_hotkey),
                game_compatibility: capture
                    .game_compatibility
                    .or(self.input.game_compatibility)
                    .unwrap_or_default(),
            },
            inject: InputInjectConfig {
                backend: inject.backend.unwrap_or(legacy_backend),
                output: inject.output.or(self.monitor).unwrap_or_default(),
            },
        };

        let audio = self.audio.unwrap_or_else(|| AudioConfig {
            enabled: legacy_role == Some(Role::Controller),
            ..AudioConfig::default()
        });

        AppConfig {
            device_name: self.device_name,
            start_with_windows: self.start_with_windows,
            preferred_role,
            transport,
            listen: self.listen,
            peer,
            layout: LayoutConfig { listener_position },
            input,
            clipboard: self.clipboard,
            audio,
        }
    }
}

impl<'de> Deserialize<'de> for AppConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(ConfigFile::deserialize(deserializer)?.into_app_config())
    }
}

impl AppConfig {
    pub fn controller_default() -> Self {
        Self {
            device_name: "Main PC".to_string(),
            start_with_windows: false,
            preferred_role: Role::Controller,
            transport: TransportMode::Connect,
            listen: None,
            peer: PeerConfig {
                name: "Linux PC".to_string(),
                host: "192.168.0.11".to_string(),
                port: default_port(),
                pinned_fingerprint: String::new(),
            },
            layout: LayoutConfig::default(),
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
            start_with_windows: false,
            preferred_role: Role::Receiver,
            transport: TransportMode::Listen,
            listen: Some("0.0.0.0:42420".to_string()),
            peer: PeerConfig::default(),
            layout: LayoutConfig::default(),
            input: InputConfig {
                inject: InputInjectConfig {
                    output: "eDP-1".to_string(),
                    ..InputInjectConfig::default()
                },
                ..InputConfig::default()
            },
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

    pub async fn load_migrating(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|source| CommonError::ReadConfig {
                    path: path.to_path_buf(),
                    source,
                })?;
        let config = toml::from_str::<Self>(&text).map_err(|source| CommonError::ParseConfig {
            path: path.to_path_buf(),
            source,
        })?;
        if config_needs_v2_migration(&text) {
            create_v1_backup(path, text.as_bytes()).await?;
            config.save(path).await?;
        }
        Ok(config)
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

fn config_needs_v2_migration(text: &str) -> bool {
    let Ok(toml::Value::Table(root)) = toml::from_str::<toml::Value>(text) else {
        return false;
    };
    root.contains_key("role")
        || root.contains_key("release_hotkey")
        || root.contains_key("monitor")
        || !root.contains_key("preferred_role")
        || !root.contains_key("transport")
        || root
            .get("peer")
            .and_then(toml::Value::as_table)
            .is_some_and(|peer| peer.contains_key("laptop"))
        || root
            .get("input")
            .and_then(toml::Value::as_table)
            .is_some_and(|input| {
                input.contains_key("backend") || input.contains_key("game_compatibility")
            })
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = OsString::from(path.as_os_str());
    backup.push(".v1.bak");
    PathBuf::from(backup)
}

async fn create_v1_backup(path: &Path, contents: &[u8]) -> Result<()> {
    let backup = backup_path(path);
    if tokio::fs::try_exists(&backup)
        .await
        .map_err(|source| CommonError::WriteConfig {
            path: backup.clone(),
            source,
        })?
    {
        return Ok(());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary_name = OsString::from(backup.as_os_str());
    temporary_name.push(format!(".tmp-{}-{nonce}", std::process::id()));
    let temporary = PathBuf::from(temporary_name);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|source| CommonError::WriteConfig {
            path: temporary.clone(),
            source,
        })?;
    if let Err(source) = file.write_all(contents).await {
        drop(file);
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(CommonError::WriteConfig {
            path: temporary,
            source,
        });
    }
    if let Err(source) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(CommonError::WriteConfig {
            path: temporary,
            source,
        });
    }
    drop(file);

    match tokio::fs::rename(&temporary, &backup).await {
        Ok(()) => Ok(()),
        Err(_) if tokio::fs::try_exists(&backup).await.unwrap_or(false) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(())
        }
        Err(source) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(CommonError::WriteConfig {
                path: backup,
                source,
            })
        }
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

        assert_eq!(actual.preferred_role, Role::Receiver);
        assert_eq!(actual.transport, TransportMode::Listen);
        assert_eq!(actual.listen.as_deref(), Some("0.0.0.0:42420"));
        assert_eq!(actual.clipboard.max_bytes, 1_048_576);
        assert!(actual.clipboard.images_enabled);
        assert_eq!(actual.clipboard.max_image_bytes, 4_194_304);
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
        let config: AppConfig = toml::from_str(
            r#"
device_name = "Legacy"
role = "controller"

[input]
backend = "auto"
"#,
        )
        .unwrap();
        assert_eq!(
            config.input.capture.game_compatibility,
            GameCompatibilityMode::AlwaysEnabled
        );
    }

    #[test]
    fn phase_zero_v1_controller_config_fixture_loads() {
        let config: AppConfig =
            toml::from_str(include_str!("../fixtures/controller-v1.toml")).unwrap();
        assert_eq!(config.preferred_role, Role::Controller);
        assert_eq!(config.transport, TransportMode::Connect);
        assert_eq!(config.input.capture.release_hotkey, "Ctrl+Alt+Pause");
        assert_eq!(config.peer.host, "192.168.0.11");
        assert_eq!(config.layout.listener_position, PeerPosition::Left);
        assert!(config.clipboard.enabled);
        assert!(config.clipboard.images_enabled);
        assert!(config.audio.enabled);
    }

    #[test]
    fn phase_zero_v1_receiver_config_fixture_loads() {
        let config: AppConfig =
            toml::from_str(include_str!("../fixtures/receiver-v1.toml")).unwrap();

        assert_eq!(config.preferred_role, Role::Receiver);
        assert_eq!(config.transport, TransportMode::Listen);
        assert_eq!(config.listen.as_deref(), Some("0.0.0.0:42420"));
        assert_eq!(config.input.inject.output, "eDP-1");
        assert_eq!(config.input.capture.release_hotkey, "Ctrl+Alt+Pause");
        assert!(config.clipboard.enabled);
        assert!(config.clipboard.images_enabled);
        assert!(!config.audio.enabled);
    }

    #[tokio::test]
    async fn legacy_config_migration_keeps_one_exact_backup_and_writes_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.toml");
        let legacy = include_str!("../fixtures/controller-v1.toml");
        tokio::fs::write(&path, legacy).await.unwrap();

        let config = AppConfig::load_migrating(&path).await.unwrap();
        let backup = tokio::fs::read_to_string(backup_path(&path)).await.unwrap();
        let migrated = tokio::fs::read_to_string(&path).await.unwrap();
        let root = toml::from_str::<toml::Value>(&migrated).unwrap();
        let root = root.as_table().unwrap();

        assert_eq!(backup, legacy);
        assert_eq!(config.transport, TransportMode::Connect);
        assert_eq!(config.peer.host, "192.168.0.11");
        assert_eq!(config.layout.listener_position, PeerPosition::Left);
        assert!(config.audio.enabled);
        assert!(config.clipboard.images_enabled);
        assert!(!root.contains_key("role"));
        assert!(!root.contains_key("release_hotkey"));
        assert!(!root.contains_key("monitor"));
        assert!(root.contains_key("preferred_role"));
        assert!(root.contains_key("transport"));
        assert!(root["peer"].as_table().unwrap().get("laptop").is_none());
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
