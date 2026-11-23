//! The editable settings draft and its validation, shared by every UI and
//! by the backend, which re-validates whatever a UI asks it to save.

use anyhow::{Context, Result};
use edge_common::{
    AppConfig, AudioLocalPlayback, AudioRoutePreference, GameCompatibilityMode, NotificationConfig,
    PeerPosition, TransportMode, parse_listen_port, update_listen_port, validate_device_name,
    validate_host, validate_port,
};
use serde::{Deserialize, Serialize};

use crate::NotificationSetup;

/// Audio buffer sizes a UI offers.
pub const AUDIO_BUFFERS_MS: [u32; 5] = [40, 60, 80, 120, 160];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    Off,
    Text,
    TextAndImages,
}

/// Everything a settings page edits. Applying it writes these into a config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDraft {
    pub device_name: String,
    /// Optional for a Linux node: with an address it dials that computer, and
    /// either way it accepts computers and phones that dial it.
    pub peer_host: String,
    /// Kept as text so a UI can hold what the user typed until validation.
    pub port: String,
    pub position: PeerPosition,
    pub game_compatibility: GameCompatibilityMode,
    pub clipboard: ClipboardMode,
    pub forward_phone_clipboard: bool,
    pub audio_route: AudioRoutePreference,
    pub audio_play_local: bool,
    pub jitter_target_ms: u32,
    pub notifications: NotificationConfig,
}

/// Why a draft can't be saved, tied to the field a UI should mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    /// The draft field, or `"draft"` when it isn't one field's fault.
    pub field: String,
    pub message: String,
}

impl SettingsDraft {
    pub fn from_config(config: &AppConfig) -> Self {
        // Both computers use one port: the one this node listens on and the
        // one it dials.
        let port = match config.transport {
            TransportMode::Connect => config.peer.port,
            TransportMode::Listen => config
                .listen
                .as_deref()
                .and_then(|listen| parse_listen_port(listen).ok())
                .unwrap_or(42_420),
        };
        Self {
            device_name: config.device_name.clone(),
            peer_host: config.peer.host.clone(),
            port: port.to_string(),
            position: config.layout.listener_position,
            game_compatibility: config.input.capture.game_compatibility,
            clipboard: match (config.clipboard.enabled, config.clipboard.images_enabled) {
                (false, _) => ClipboardMode::Off,
                (true, false) => ClipboardMode::Text,
                (true, true) => ClipboardMode::TextAndImages,
            },
            forward_phone_clipboard: config.phone.forward_clipboard_to_pcs,
            audio_route: config.audio.route.unwrap_or(if config.audio.enabled {
                AudioRoutePreference::PeerToLocal
            } else {
                AudioRoutePreference::Disabled
            }),
            audio_play_local: config.audio.local_playback == AudioLocalPlayback::Mirror,
            jitter_target_ms: config.audio.jitter_target_ms,
            notifications: config.notifications.clone(),
        }
    }

    /// Writes the draft into a copy of `original`. `connector_only` is for the
    /// Windows app, which can only dial, so it needs an address.
    pub fn apply(&self, original: &AppConfig, connector_only: bool) -> Result<AppConfig> {
        validate_device_name(&self.device_name)?;
        let port = self.parsed_port()?;

        let mut config = original.clone();
        config.device_name = self.device_name.trim().to_string();
        config.input.capture.game_compatibility = self.game_compatibility;
        config.clipboard.enabled = self.clipboard != ClipboardMode::Off;
        config.clipboard.images_enabled = self.clipboard == ClipboardMode::TextAndImages;
        config.phone.forward_clipboard_to_pcs = self.forward_phone_clipboard;
        config.audio.route = Some(self.audio_route);
        config.audio.enabled = self.audio_route != AudioRoutePreference::Disabled;
        config.audio.local_playback = if self.audio_play_local {
            AudioLocalPlayback::Mirror
        } else {
            AudioLocalPlayback::Redirect
        };
        config.audio.jitter_target_ms = self.jitter_target_ms;
        config.notifications = self.notifications.clone();
        let host = self.peer_host.trim();
        if connector_only || !host.is_empty() {
            validate_host(host)?;
        }
        config.peer.host = host.to_string();
        config.peer.port = port;
        config.layout.listener_position = self.position;
        if !connector_only {
            config.listen = Some(update_listen_port(config.listen.as_deref(), port));
        }
        // Kept for older builds and the Windows app; a Linux node listens
        // either way and dials whenever it has an address.
        config.transport = if connector_only || !host.is_empty() {
            TransportMode::Connect
        } else {
            TransportMode::Listen
        };
        Ok(config)
    }

    fn parsed_port(&self) -> Result<u16> {
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .ok()
            .context("port must be a number between 1 and 65535")?;
        validate_port(port)?;
        Ok(port)
    }

    /// Every problem with the draft, each tied to its field. Empty means
    /// [`apply`](Self::apply) succeeds.
    pub fn validate(
        &self,
        connector_only: bool,
        saved_notifications: &NotificationConfig,
        setup: Option<&NotificationSetup>,
    ) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let mut push = |field: &str, error: &dyn std::fmt::Display| {
            errors.push(FieldError {
                field: field.to_string(),
                message: error.to_string(),
            });
        };
        if let Err(error) = validate_device_name(&self.device_name) {
            push("device_name", &error);
        }
        if let Err(error) = self.parsed_port() {
            push("port", &error);
        }
        let host = self.peer_host.trim();
        if (connector_only || !host.is_empty())
            && let Err(error) = validate_host(host)
        {
            push("peer_host", &error);
        }
        if !AUDIO_BUFFERS_MS.contains(&self.jitter_target_ms) {
            push(
                "jitter_target_ms",
                &format!("audio buffer must be one of {AUDIO_BUFFERS_MS:?} ms"),
            );
        }
        if self.notifications != *saved_notifications
            && let Err(error) = validate_notification_selection(&self.notifications, setup)
        {
            push("notifications", &error);
        }
        errors
    }
}

/// Notification enrollment has to name paired computers and keep history
/// with its current owner.
pub fn validate_notification_selection(
    config: &NotificationConfig,
    setup: Option<&NotificationSetup>,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let setup = setup.context("Connect to the running node before enabling notifications")?;
    anyhow::ensure!(
        !config.peers.is_empty(),
        "Choose a paired computer to share notifications with"
    );
    anyhow::ensure!(
        config
            .peers
            .iter()
            .all(|id| setup.computers.iter().any(|p| &p.fingerprint == id)),
        "Only paired computers can share notifications; remove any unpaired devices from the selection"
    );
    anyhow::ensure!(
        config.coordinator_fingerprint == setup.identity
            || config.peers.contains(&config.coordinator_fingerprint),
        "Choose an enrolled computer to store history"
    );
    anyhow::ensure!(
        !setup.has_store || config.coordinator_fingerprint == setup.coordinator,
        "History already belongs to another computer; changing its owner requires migration"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_survives_a_draft_round_trip() {
        let config = AppConfig::receiver_default();
        let draft = SettingsDraft::from_config(&config);
        let applied = draft.apply(&config, false).unwrap();
        assert_eq!(SettingsDraft::from_config(&applied), draft);
    }

    #[test]
    fn validation_names_each_bad_field() {
        let config = AppConfig::receiver_default();
        let mut draft = SettingsDraft::from_config(&config);
        draft.device_name = "  ".into();
        draft.port = "70000".into();
        draft.jitter_target_ms = 55;
        let fields: Vec<_> = draft
            .validate(false, &config.notifications, None)
            .into_iter()
            .map(|error| error.field)
            .collect();
        assert_eq!(fields, ["device_name", "port", "jitter_target_ms"]);
        assert!(draft.apply(&config, false).is_err());
    }

    #[test]
    fn the_windows_app_needs_an_address() {
        let config = AppConfig::controller_default();
        let mut draft = SettingsDraft::from_config(&config);
        draft.peer_host = String::new();
        assert!(
            draft
                .validate(true, &config.notifications, None)
                .iter()
                .any(|error| error.field == "peer_host")
        );
        assert!(
            draft
                .validate(false, &config.notifications, None)
                .is_empty()
        );
    }

    #[test]
    fn unchanged_notifications_are_not_revalidated() {
        let mut config = AppConfig::receiver_default();
        config.notifications.enabled = true;
        let draft = SettingsDraft::from_config(&config);
        // No setup to check against, but nothing about notifications changed.
        assert!(
            draft
                .validate(false, &config.notifications, None)
                .is_empty()
        );
    }
}
