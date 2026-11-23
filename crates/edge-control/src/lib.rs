//! The local control contract between an edge-kvm backend and the desktop
//! UIs that drive it: UI-independent status types, the settings draft and
//! its validation, and a versioned, bounded, length-prefixed protocol.
//!
//! The backend owns sessions, identity, pins, configuration writes and
//! permissions. A UI only ever asks; it never edits those files itself.

mod draft;
pub mod endpoint;
pub mod framing;
pub mod protocol;
mod status;
pub mod u64_string;

#[cfg(feature = "tokio")]
pub mod client;

pub use draft::{
    AUDIO_BUFFERS_MS, ClipboardMode, FieldError, SettingsDraft, validate_notification_selection,
};
pub use status::{NodeStatus, NotificationComputer, NotificationSetup, PhoneEntry, PhonePairingEvent};
