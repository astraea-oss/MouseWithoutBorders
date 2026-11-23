//! The local control protocol between a backend and its desktop UI.
//!
//! A client opens the endpoint and writes [`PREAMBLE`] followed by the
//! protocol version it speaks (u32, big endian). Older text commands never
//! start with a NUL byte, so one endpoint serves both during the migration.
//! After the preamble every message is a length-prefixed JSON frame (see
//! [`crate::framing`]).
//!
//! The server answers the preamble with [`ServerMessage::Hello`], or with
//! [`ServerMessage::Incompatible`] and closes. The client then sends
//! [`ClientMessage`]s; each gets exactly one [`ServerMessage::Response`] with
//! the same `id`. After a successful [`Request::Subscribe`], the server also
//! pushes [`ServerMessage::Event`]s until the connection closes.
//!
//! Reconnecting never replays anything: a client that reconnects asks for a
//! fresh snapshot, and pending phone pairings or approvals from the old
//! connection are cancelled by the server when it closes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{FieldError, NotificationSetup, SettingsDraft};

/// Written by the client before anything else.
pub const PREAMBLE: [u8; 4] = *b"\0HKC";
/// The version this crate speaks. Bump it for any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Identifies the backend instance a UI is attached to, so a UI started from
/// one portable folder never silently drives a backend from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The backend's `CARGO_PKG_VERSION`.
    pub backend_version: String,
    /// `"linux"` or `"windows"`.
    pub platform: String,
    pub pid: u32,
    pub executable: PathBuf,
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub log_path: PathBuf,
    /// Run by the user service manager rather than started by hand.
    pub service_managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMessage {
    /// Chosen by the client; echoed in the response. Fits a JS number.
    pub id: u32,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Reply [`Reply::Snapshot`], then push events on this connection.
    Subscribe,
    Snapshot,
    /// Share this computer's mouse and keyboard with the connected computer.
    SetInputForwarding {
        enabled: bool,
    },
    SetController {
        controller: Controller,
    },
    SetAudio {
        target: AudioTarget,
    },
    /// Pause the computer connection without quitting.
    Disconnect,
    Reconnect,
    /// Allow a new computer to pair (both computers confirm).
    ArmComputerPairing,
    /// Reply [`Reply::PhonePairing`]; the outcome arrives as
    /// [`Event::PhonePairing`]. Closing the connection cancels it.
    StartPhonePairing,
    CancelPhonePairing,
    UnpairPhone {
        name: String,
    },
    /// Reply [`Reply::Settings`].
    Settings,
    /// Reply [`Reply::Validation`]. Writes nothing.
    ValidateSettings {
        draft: SettingsDraft,
    },
    /// Validate, write and restart the backend. Reply [`Reply::Saved`], or
    /// [`ErrorKind::Invalid`] with the persisted settings left untouched.
    SaveSettings {
        draft: SettingsDraft,
        /// The draft's starting point; a save is refused if the file changed
        /// since, so a stale window can't overwrite newer settings.
        base: SettingsDraft,
    },
    /// Reply [`Reply::Autostart`].
    Autostart,
    SetAutostart {
        enabled: bool,
    },
    /// Reply [`Reply::Notifications`].
    Notifications,
    InstallNotificationBridge {
        shell_path: PathBuf,
    },
    /// Reply [`Reply::Diagnostics`].
    Diagnostics,
    /// Stop the backend.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Controller {
    /// This computer's mouse and keyboard drive the other computer.
    Local,
    /// The other computer drives this one.
    Peer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioTarget {
    Off,
    /// This computer's audio plays on the connected computer.
    ToPeer,
    /// The connected computer's audio plays here.
    FromPeer,
    /// This computer's audio plays on a connected phone.
    ToPhone {
        fingerprint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello(Hello),
    /// The server can't speak the client's version. It closes after this.
    Incompatible {
        server_protocol: u32,
    },
    Response {
        id: u32,
        #[serde(flatten)]
        outcome: Outcome,
    },
    Event {
        event: Event,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Reply),
    Error(ControlError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Done,
    Snapshot(Box<Snapshot>),
    PhonePairing(PhonePairingOffer),
    Settings(Box<SettingsView>),
    Validation {
        errors: Vec<FieldError>,
    },
    Saved {
        restarting: bool,
    },
    /// `None` when starting at login can't be set on this installation.
    Autostart {
        enabled: Option<bool>,
    },
    Notifications {
        setup: Option<NotificationSetup>,
    },
    Message {
        text: String,
    },
    Diagnostics {
        report: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ControlError {
    pub kind: ErrorKind,
    pub message: String,
    /// Field problems for [`ErrorKind::Invalid`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Not possible right now (no computer connected, a switch in progress…).
    Unavailable,
    /// This backend or platform doesn't offer it.
    Unsupported,
    Invalid,
    /// Someone else changed the settings since the draft was made.
    Conflict,
    NotFound,
    Busy,
    /// The request couldn't be parsed.
    BadRequest,
    Failed,
}

impl ControlError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            fields: Vec::new(),
        }
    }

    pub fn invalid(fields: Vec<FieldError>) -> Self {
        let message = fields
            .first()
            .map(|field| field.message.clone())
            .unwrap_or_else(|| "invalid settings".to_string());
        Self {
            kind: ErrorKind::Invalid,
            message,
            fields,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Snapshot(Box<Snapshot>),
    PhonePairing(PhonePairingUpdate),
    /// The backend is about to stop or restart; the UI should reconnect.
    Stopping {
        restarting: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhonePairingOffer {
    pub uri: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PhonePairingUpdate {
    Paired {
        name: String,
    },
    /// Expired, cancelled or replaced by a newer pairing.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Starting,
    /// Waiting for a computer.
    Listening,
    Connected,
    /// Paused by the user.
    Paused,
    Error,
}

/// Everything Home shows, published whole on every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Increases with every published change.
    #[serde(with = "crate::u64_string")]
    pub revision: u64,
    pub state: LinkState,
    pub summary: String,
    pub local_name: Option<String>,
    pub peer_name: Option<String>,
    pub round_trip_ms: Option<u32>,
    pub controller: Controller,
    pub can_switch_controller: bool,
    pub switching_controller: bool,
    pub input_forwarding: bool,
    pub remote_input_active: bool,
    pub audio: AudioState,
    pub pairing_armed: bool,
    /// Whether this backend serves phones at all.
    pub phones_supported: bool,
    pub phones: Vec<PhoneState>,
    pub last_error: Option<String>,
    pub capabilities: Capabilities,
    pub diagnostics: Vec<Diagnostic>,
}

impl Default for Snapshot {
    /// A node that hasn't reported anything yet.
    fn default() -> Self {
        Self {
            revision: 0,
            state: LinkState::Starting,
            summary: String::new(),
            local_name: None,
            peer_name: None,
            round_trip_ms: None,
            controller: Controller::Local,
            can_switch_controller: false,
            switching_controller: false,
            input_forwarding: false,
            remote_input_active: false,
            audio: AudioState {
                target: AudioTarget::Off,
                from_phone: None,
                to_peer_available: false,
                from_peer_available: false,
            },
            pairing_armed: false,
            phones_supported: false,
            phones: Vec::new(),
            last_error: None,
            capabilities: Capabilities::default(),
            diagnostics: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioState {
    pub target: AudioTarget,
    /// From a phone; only the phone can start or change this.
    pub from_phone: Option<String>,
    pub to_peer_available: bool,
    pub from_peer_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneState {
    /// Empty for a paired phone that isn't connected; the backend reports
    /// those by name only.
    pub fingerprint: String,
    pub name: String,
    pub connected: bool,
    pub round_trip_ms: Option<u32>,
    pub accepts_audio: bool,
}

/// What this backend can do, so a UI shows unavailable controls as such
/// instead of guessing from the platform.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub phones: bool,
    pub notifications: bool,
    pub autostart: bool,
    /// Accepts computers that dial it as well as dialling out.
    pub listens: bool,
    /// Sharing this computer's screen with View. False until View lands.
    pub view_host: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub key: String,
    pub value: String,
}

/// The persisted settings as a draft, plus what a UI needs to edit them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    pub draft: SettingsDraft,
    pub config_path: PathBuf,
    /// The Windows app can only dial, so it needs an address.
    pub connector_only: bool,
    pub local_address: Option<String>,
    pub audio_buffers_ms: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_carry_their_id_beside_the_outcome() {
        let message = ServerMessage::Response {
            id: 7,
            outcome: Outcome::Ok(Reply::Saved { restarting: true }),
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "response", "id": 7, "ok": {"type": "saved", "restarting": true}})
        );
        assert_eq!(
            serde_json::from_value::<ServerMessage>(json).unwrap(),
            message
        );
    }

    #[test]
    fn errors_round_trip_with_their_fields() {
        let message = ServerMessage::Response {
            id: 1,
            outcome: Outcome::Error(ControlError::invalid(vec![FieldError {
                field: "port".into(),
                message: "bad port".into(),
            }])),
        };
        let text = serde_json::to_string(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&text).unwrap(),
            message
        );
    }

    #[test]
    fn requests_are_tagged() {
        let json = serde_json::json!({"id": 3, "request": {"type": "set_audio", "target": {"kind": "to_phone", "fingerprint": "ab"}}});
        let message: ClientMessage = serde_json::from_value(json).unwrap();
        assert_eq!(
            message.request,
            Request::SetAudio {
                target: AudioTarget::ToPhone {
                    fingerprint: "ab".into()
                }
            }
        );
    }

    #[test]
    fn unknown_requests_fail_to_parse() {
        let json = serde_json::json!({"id": 3, "request": {"type": "format_disk"}});
        assert!(serde_json::from_value::<ClientMessage>(json).is_err());
    }
}
