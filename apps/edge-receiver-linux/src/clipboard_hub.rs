//! One Wayland clipboard watcher shared by every session.
//!
//! Sessions keep their own per-peer clipboard state (sequence numbers, image
//! transfers, and the record of what that peer has already seen, which stops
//! echoes) and subscribe here for change notifications. The watcher outlives
//! sessions, so peers connecting and disconnecting don't restart it.
//!
//! The hub also remembers what a phone last put on the clipboard, so PC
//! sessions can hold it back when phone content mustn't reach other PCs.

use std::sync::{Arc, Mutex};

use edge_clipboard::ClipboardContentId;
use tokio::sync::watch;

use crate::AbortOnDropTask;

/// The content a phone most recently wrote to the local clipboard.
#[derive(Clone, Default)]
pub(crate) struct PhoneOrigin(Arc<Mutex<Option<ClipboardContentId>>>);

impl PhoneOrigin {
    pub fn record(&self, id: ClipboardContentId) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(id);
    }

    pub fn is_from_phone(&self, id: &ClipboardContentId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            == Some(id)
    }
}

pub(crate) struct ClipboardHub {
    changes: watch::Receiver<u64>,
    origin: PhoneOrigin,
    _task: AbortOnDropTask,
}

impl ClipboardHub {
    pub fn spawn() -> Self {
        let (sender, changes) = watch::channel(0_u64);
        let task = tokio::spawn(async move {
            // The watcher restarts `wl-paste` itself; it only ends if its task
            // dies, in which case start a fresh one.
            loop {
                let mut watcher = edge_linux_input::spawn_clipboard_change_watcher();
                while watcher.recv().await.is_some() {
                    sender.send_modify(|generation| *generation = generation.wrapping_add(1));
                }
                tracing::warn!("Wayland clipboard watcher stopped; restarting");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        Self {
            changes,
            origin: PhoneOrigin::default(),
            _task: AbortOnDropTask(task),
        }
    }

    /// Changes after this call. Bursts between two reads arrive as one
    /// notification, as they did with a per-session watcher.
    pub fn subscribe(&self) -> ClipboardChanges {
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        ClipboardChanges {
            changes,
            origin: self.origin.clone(),
        }
    }
}

pub(crate) struct ClipboardChanges {
    changes: watch::Receiver<u64>,
    origin: PhoneOrigin,
}

impl ClipboardChanges {
    /// Waits for the next clipboard change. `false` means the hub is gone.
    pub async fn changed(&mut self) -> bool {
        self.changes.changed().await.is_ok()
    }

    /// Another subscription to the same hub, starting from now.
    pub fn resubscribe(&self) -> Self {
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        Self {
            changes,
            origin: self.origin.clone(),
        }
    }

    pub fn phone_origin(&self) -> PhoneOrigin {
        self.origin.clone()
    }
}

#[cfg(test)]
impl ClipboardHub {
    /// A hub driven by the test instead of `wl-paste`.
    fn manual() -> (Self, watch::Sender<u64>) {
        let (sender, changes) = watch::channel(0_u64);
        let hub = Self {
            changes,
            origin: PhoneOrigin::default(),
            _task: AbortOnDropTask(tokio::spawn(std::future::pending())),
        };
        (hub, sender)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time;

    use super::*;

    async fn notified(changes: &mut ClipboardChanges) -> bool {
        time::timeout(Duration::from_millis(200), changes.changed())
            .await
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn every_subscriber_sees_a_change() {
        let (hub, sender) = ClipboardHub::manual();
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();

        sender.send_modify(|generation| *generation += 1);

        assert!(notified(&mut first).await);
        assert!(notified(&mut second).await);
    }

    #[tokio::test]
    async fn new_subscribers_ignore_earlier_changes() {
        let (hub, sender) = ClipboardHub::manual();
        sender.send_modify(|generation| *generation += 1);

        let mut late = hub.subscribe();

        assert!(!notified(&mut late).await);
    }

    #[tokio::test]
    async fn a_burst_of_changes_is_one_notification() {
        let (hub, sender) = ClipboardHub::manual();
        let mut changes = hub.subscribe();
        for _ in 0..5 {
            sender.send_modify(|generation| *generation += 1);
        }

        assert!(notified(&mut changes).await);
        assert!(!notified(&mut changes).await);
    }

    #[tokio::test]
    async fn subscribers_learn_when_the_hub_is_gone() {
        let (hub, sender) = ClipboardHub::manual();
        let mut changes = hub.subscribe();
        drop(sender);

        assert!(!changes.changed().await);
    }

    #[test]
    fn subscribers_share_the_phone_origin() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (hub, _sender) = ClipboardHub::manual();
        let phone = hub.subscribe();
        let pc = hub.subscribe().resubscribe();
        let copied = ClipboardContentId::Text([7; 32]);

        phone.phone_origin().record(copied);

        assert!(pc.phone_origin().is_from_phone(&copied));
        assert!(
            !pc.phone_origin()
                .is_from_phone(&ClipboardContentId::Text([8; 32]))
        );
    }
}
