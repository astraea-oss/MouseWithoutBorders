//! What a running node reports about itself, independent of any UI toolkit.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

/// The legacy status view the egui windows show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeStatus {
    pub summary: String,
    pub connected: bool,
    pub peer: Option<String>,
    pub round_trip_ms: Option<u64>,
    pub diagnostics: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneEntry {
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhonePairingEvent {
    /// Show this link as a QR code until it expires.
    Offer {
        uri: String,
        expires_in: Duration,
    },
    Paired(String),
    /// Expired or replaced by a newer pairing.
    Closed,
    Failed(String),
}

/// Linux notification sharing capabilities and current health.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSetup {
    pub identity: String,
    pub computers: Vec<NotificationComputer>,
    pub status: String,
    pub active: bool,
    pub has_store: bool,
    pub coordinator: String,
    pub shell_path: Option<PathBuf>,
    pub socket_path: PathBuf,
    pub bridge_installed: bool,
    #[serde(default)]
    pub bridge_connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationComputer {
    pub name: String,
    pub fingerprint: String,
}
