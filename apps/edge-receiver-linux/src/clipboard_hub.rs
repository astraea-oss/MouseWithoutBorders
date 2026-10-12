//! One Wayland clipboard watcher shared by every session.
//!
//! Sessions keep their own per-peer clipboard state (sequence numbers, image
//! transfers, and the record of what that peer has already seen, which stops
//! echoes) and subscribe here for change notifications. The watcher outlives
//! sessions, so peers connecting and disconnecting don't restart it.

use tokio::sync::watch;

use crate::AbortOnDropTask;

pub(crate) struct ClipboardHub {
    changes: watch::Receiver<u64>,
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
            _task: AbortOnDropTask(task),
        }
    }

    /// Changes after this call. Bursts between two reads arrive as one
    /// notification, as they did with a per-session watcher.
    pub fn subscribe(&self) -> ClipboardChanges {
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        ClipboardChanges(changes)
    }
}

pub(crate) struct ClipboardChanges(watch::Receiver<u64>);

impl ClipboardChanges {
    /// Waits for the next clipboard change. `false` means the hub is gone.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

#[cfg(test)]
impl ClipboardHub {
    /// A hub driven by the test instead of `wl-paste`.
    fn manual() -> (Self, watch::Sender<u64>) {
        let (sender, changes) = watch::channel(0_u64);
        let hub = Self {
            changes,
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
}
