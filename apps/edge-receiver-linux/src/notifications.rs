//! Shows a phone's notifications on the desktop (view only).
//!
//! Each phone has a [`NotificationMirror`] that outlives its connections and
//! maps the phone's notification ids to desktop notification ids, so updates
//! replace the desktop notification in place and removals close it. After a
//! reconnect the phone resends everything current between `SnapshotStart` and
//! `SnapshotEnd`; unchanged notifications stay as they are, and anything not
//! resent is closed.
//!
//! Notifications go to `org.freedesktop.Notifications` on the session bus.

use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use edge_protocol::{NotificationEvent, PhoneNotification};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zbus::zvariant::Value;

/// What the desktop is asked to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DesktopNotification {
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub icon_png: Option<Vec<u8>>,
}

/// The desktop's notification service.
pub(crate) trait Desktop {
    /// Session bus + notification daemon identity; numeric IDs only belong to this generation.
    async fn generation(&self) -> Result<String>;
    /// Shows a notification, replacing `replaces` if it's non-zero. Returns
    /// the desktop's id for it.
    async fn show(
        &self,
        notification: &DesktopNotification,
        replaces: u32,
        generation: &str,
    ) -> Result<u32>;
    async fn close(&self, id: u32, generation: &str) -> Result<()>;
}

/// Turns a phone notification into what the desktop shows: limits enforced
/// again here, and text escaped because the desktop renders markup.
pub(crate) fn desktop_notification(
    phone: &PhoneNotification,
    phone_name: &str,
) -> DesktopNotification {
    let app = truncate_chars(&phone.app_name, PhoneNotification::MAX_APP_NAME_CHARS);
    let title = truncate_chars(&phone.title, PhoneNotification::MAX_TITLE_CHARS);
    let summary = if title.trim().is_empty() {
        app.clone()
    } else {
        title
    };
    DesktopNotification {
        app_name: format!("{app} ({phone_name})"),
        summary: escape_markup(&summary),
        body: escape_markup(&truncate_bytes(
            &phone.body,
            PhoneNotification::MAX_BODY_BYTES,
        )),
        icon_png: phone
            .icon_png
            .clone()
            .filter(|png| png.len() <= PhoneNotification::MAX_ICON_BYTES),
    }
}

#[derive(Serialize, Deserialize)]
struct Shown {
    desktop_id: u32,
    digest: u64,
}

#[derive(Default)]
pub(crate) struct NotificationMirror {
    shown: HashMap<u64, Shown>,
    /// Phone ids posted since the current `SnapshotStart`.
    resent: Option<HashSet<u64>>,
    owner: String,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SavedMirror {
    owner: String,
    shown: HashMap<u64, Shown>,
}

impl NotificationMirror {
    /// Reuse retained desktop entries after a backend restart. This cache contains
    /// only IDs and content digests, never notification text or icons.
    pub async fn restore(&mut self, desktop: &impl Desktop, path: &Path) -> Result<()> {
        if self.path.is_some() {
            return Ok(());
        }
        let owner = desktop.generation().await?;
        let saved = match tokio::fs::metadata(path).await {
            Ok(meta) => {
                anyhow::ensure!(
                    meta.len() <= 1024 * 1024,
                    "phone notification cache too large"
                );
                Some(serde_json::from_slice::<SavedMirror>(
                    &tokio::fs::read(path).await?,
                )?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(saved) = saved
            && saved.owner == owner
        {
            self.shown = saved.shown;
        }
        self.owner = owner;
        self.path = Some(path.into());
        Ok(())
    }

    async fn sync_owner(&mut self, desktop: &impl Desktop) -> Result<()> {
        let owner = desktop.generation().await?;
        if self.owner != owner {
            self.shown.clear();
            self.resent = None;
            self.owner = owner;
        }
        Ok(())
    }

    async fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .context("phone notification cache directory")?;
        tokio::fs::create_dir_all(parent).await?;
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
        let data =
            serde_json::to_vec(&serde_json::json!({ "owner": self.owner, "shown": self.shown }))?;
        anyhow::ensure!(
            data.len() <= 1024 * 1024,
            "phone notification cache too large"
        );
        let temporary = path.with_extension("pending");
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .await?;
        file.write_all(&data).await?;
        file.sync_all().await?;
        tokio::fs::rename(temporary, path).await?;
        let parent = parent.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
        Ok(())
    }

    pub async fn apply(
        &mut self,
        event: NotificationEvent,
        desktop: &impl Desktop,
        phone_name: &str,
    ) -> Result<()> {
        self.sync_owner(desktop).await?;
        match event {
            NotificationEvent::Posted(notification) => {
                if let Some(resent) = &mut self.resent {
                    resent.insert(notification.id);
                }
                let shown = desktop_notification(&notification, phone_name);
                let digest = digest(&shown);
                let replaces = match self.shown.get(&notification.id) {
                    Some(existing) if existing.digest == digest => return Ok(()),
                    Some(existing) => existing.desktop_id,
                    None => 0,
                };
                let desktop_id = desktop.show(&shown, replaces, &self.owner).await?;
                self.shown
                    .insert(notification.id, Shown { desktop_id, digest });
            }
            NotificationEvent::Removed { id } => {
                if let Some(shown) = self.shown.remove(&id) {
                    desktop.close(shown.desktop_id, &self.owner).await?;
                }
            }
            NotificationEvent::SnapshotStart => self.resent = Some(HashSet::new()),
            NotificationEvent::SnapshotEnd => {
                let Some(resent) = self.resent.take() else {
                    return Ok(());
                };
                let gone: Vec<u64> = self
                    .shown
                    .keys()
                    .filter(|id| !resent.contains(id))
                    .copied()
                    .collect();
                for id in gone {
                    if let Some(shown) = self.shown.remove(&id) {
                        desktop.close(shown.desktop_id, &self.owner).await?;
                    }
                }
            }
        }
        self.persist().await?;
        Ok(())
    }

    /// Closes everything this phone has on the desktop, e.g. after unpairing.
    pub async fn close_all(&mut self, desktop: &impl Desktop) {
        // Never close a recycled numeric ID owned by a new daemon.
        if self.sync_owner(desktop).await.is_err() {
            return;
        }
        for (_, shown) in self.shown.drain() {
            desktop.close(shown.desktop_id, &self.owner).await.ok();
        }
        self.resent = None;
        if let Err(error) = self.persist().await {
            tracing::warn!(%error, "couldn't save phone notification removals");
        }
    }
}

fn digest(notification: &DesktopNotification) -> u64 {
    let mut hasher = DefaultHasher::new();
    notification.hash(&mut hasher);
    hasher.finish()
}

impl Hash for DesktopNotification {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.app_name.hash(state);
        self.summary.hash(state);
        self.body.hash(state);
        self.icon_png.hash(state);
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

pub(crate) fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// `org.freedesktop.Notifications` on the session bus. Icons are written to
/// the runtime directory, named by content, and passed as file paths.
pub(crate) struct FreedesktopNotifications {
    connection: zbus::Connection,
    icon_dir: PathBuf,
    bus_id: String,
}

impl FreedesktopNotifications {
    pub async fn close_shared(&self, id: u32) -> Result<()> {
        self.close(id, &self.generation().await?).await
    }

    pub async fn show_shared(
        &self,
        key: &edge_protocol::NotificationKey,
        content: &edge_protocol::SharedNotification,
        replaces: u32,
        _local_name: &str,
        silent: bool,
    ) -> Result<u32> {
        let mut hints: HashMap<&str, Value> = HashMap::new();
        let key_json = serde_json::to_string(key)?;
        hints.insert("x-edge-kvm-key", Value::from(key_json.as_str()));
        // Strings avoid JavaScript's integer precision limit.
        let posted = content.posted_ms.to_string();
        hints.insert("x-edge-kvm-posted-ms", Value::from(posted.as_str()));
        if silent {
            hints.insert("x-edge-kvm-replay", Value::from(true));
            hints.insert("suppress-sound", Value::from(true));
        }
        hints.insert("urgency", Value::from(content.urgency));
        let icon = content
            .icon_png
            .as_ref()
            .map(|png| self.icon_path(png))
            .transpose()?
            .unwrap_or_else(|| content.icon.clone());
        let reply = self
            .connection
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    content.app_name.as_str(),
                    replaces,
                    icon.as_str(),
                    content.title.as_str(),
                    content.body.as_str(),
                    Vec::<&str>::new(),
                    hints,
                    0_i32,
                ),
            )
            .await?;
        Ok(reply.body().deserialize::<u32>()?)
    }

    pub async fn connect(icon_dir: PathBuf) -> Result<Self> {
        let connection = zbus::Connection::session()
            .await
            .context("no session bus for desktop notifications")?;
        let bus_id: String = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetId",
                &(),
            )
            .await?
            .body()
            .deserialize()?;
        Ok(Self {
            connection,
            icon_dir,
            bus_id,
        })
    }

    fn icon_path(&self, png: &[u8]) -> Result<String> {
        let path = self
            .icon_dir
            .join(format!("{}.png", hex::encode(Sha256::digest(png))));
        if !path.exists() {
            std::fs::create_dir_all(&self.icon_dir)?;
            std::fs::write(&path, png)?;
        }
        Ok(path.display().to_string())
    }
}

impl Desktop for FreedesktopNotifications {
    async fn generation(&self) -> Result<String> {
        let owner: String = self
            .connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &("org.freedesktop.Notifications",),
            )
            .await?
            .body()
            .deserialize()?;
        Ok(format!("{}/{}", self.bus_id, owner))
    }

    async fn show(
        &self,
        notification: &DesktopNotification,
        replaces: u32,
        generation: &str,
    ) -> Result<u32> {
        let icon = match &notification.icon_png {
            Some(png) => self.icon_path(png).unwrap_or_default(),
            None => String::new(),
        };
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("category", Value::from("im.received"));
        // Legacy phone mirroring has no onward-sharing permission yet.
        hints.insert("x-edge-kvm-phone", Value::from(true));
        if !icon.is_empty() {
            hints.insert("image-path", Value::from(icon.as_str()));
        }
        let reply = self
            .connection
            .call_method(
                Some(
                    generation
                        .rsplit_once('/')
                        .context("invalid notification daemon identity")?
                        .1,
                ),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    notification.app_name.as_str(),
                    replaces,
                    icon.as_str(),
                    notification.summary.as_str(),
                    notification.body.as_str(),
                    Vec::<&str>::new(),
                    hints,
                    -1_i32,
                ),
            )
            .await
            .context("the desktop refused the notification")?;
        Ok(reply.body().deserialize::<u32>()?)
    }

    async fn close(&self, id: u32, generation: &str) -> Result<()> {
        self.connection
            .call_method(
                Some(
                    generation
                        .rsplit_once('/')
                        .context("invalid notification daemon identity")?
                        .1,
                ),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id,),
            )
            .await
            .context("couldn't close the desktop notification")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records what would have been shown.
    #[derive(Default)]
    struct FakeDesktop {
        owner: Mutex<String>,
        next_id: Mutex<u32>,
        shown: Mutex<HashMap<u32, DesktopNotification>>,
        calls: Mutex<Vec<String>>,
    }

    impl Desktop for FakeDesktop {
        async fn generation(&self) -> Result<String> {
            Ok(self.owner.lock().unwrap().clone())
        }
        async fn show(
            &self,
            notification: &DesktopNotification,
            replaces: u32,
            _generation: &str,
        ) -> Result<u32> {
            let id = if replaces == 0 {
                let mut next = self.next_id.lock().unwrap();
                *next += 1;
                *next
            } else {
                replaces
            };
            self.calls.lock().unwrap().push(format!("show {id}"));
            self.shown.lock().unwrap().insert(id, notification.clone());
            Ok(id)
        }

        async fn close(&self, id: u32, _generation: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("close {id}"));
            self.shown.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    impl FakeDesktop {
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }

        fn titles(&self) -> Vec<String> {
            let mut titles: Vec<String> = self
                .shown
                .lock()
                .unwrap()
                .values()
                .map(|shown| shown.summary.clone())
                .collect();
            titles.sort();
            titles
        }
    }

    fn posted(id: u64, title: &str) -> NotificationEvent {
        NotificationEvent::Posted(PhoneNotification {
            id,
            app_name: "Messages".into(),
            title: title.into(),
            body: "hello".into(),
            icon_png: None,
            posted_ms: 0,
        })
    }

    async fn apply(
        mirror: &mut NotificationMirror,
        desktop: &FakeDesktop,
        events: Vec<NotificationEvent>,
    ) {
        for event in events {
            mirror.apply(event, desktop, "Pixel").await.unwrap();
        }
    }

    #[tokio::test]
    async fn backend_restart_reuses_phone_entries_without_merging_distinct_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/phone.json");
        let desktop = FakeDesktop::default();
        *desktop.owner.lock().unwrap() = "bus-a/daemon-a".into();
        let mut mirror = NotificationMirror::default();
        mirror.restore(&desktop, &path).await.unwrap();
        apply(
            &mut mirror,
            &desktop,
            vec![posted(7, "Same text"), posted(8, "Same text")],
        )
        .await;
        assert_eq!(desktop.titles().len(), 2);
        desktop.calls();
        drop(mirror);
        let mut restored = NotificationMirror::default();
        restored.restore(&desktop, &path).await.unwrap();
        apply(
            &mut restored,
            &desktop,
            vec![
                NotificationEvent::SnapshotStart,
                posted(7, "Same text"),
                posted(8, "Same text"),
                NotificationEvent::SnapshotEnd,
            ],
        )
        .await;
        assert!(desktop.calls().is_empty());
        assert_eq!(desktop.titles().len(), 2);
        apply(&mut restored, &desktop, vec![posted(7, "Updated")]).await;
        assert_eq!(desktop.calls(), ["show 1"]);
        assert_eq!(desktop.titles().len(), 2);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("Same text") && !saved.contains("Updated") && !saved.contains("hello")
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[tokio::test]
    async fn daemon_or_bus_restart_never_replaces_an_unrelated_recycled_id() {
        for restart_backend in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("phone.json");
            let desktop = FakeDesktop::default();
            *desktop.owner.lock().unwrap() = "bus-a/daemon-a".into();
            let mut mirror = NotificationMirror::default();
            mirror.restore(&desktop, &path).await.unwrap();
            apply(&mut mirror, &desktop, vec![posted(7, "Phone message")]).await;
            *desktop.owner.lock().unwrap() = "bus-b/daemon-a".into();
            *desktop.next_id.lock().unwrap() = 0;
            desktop.shown.lock().unwrap().clear();
            let unrelated = DesktopNotification {
                app_name: "Other app".into(),
                summary: "Keep me".into(),
                body: String::new(),
                icon_png: None,
            };
            desktop.show(&unrelated, 0, "bus-b/daemon-a").await.unwrap();
            if restart_backend {
                mirror = NotificationMirror::default();
                mirror.restore(&desktop, &path).await.unwrap();
            }
            apply(
                &mut mirror,
                &desktop,
                vec![
                    NotificationEvent::SnapshotStart,
                    posted(7, "Phone message"),
                    NotificationEvent::SnapshotEnd,
                ],
            )
            .await;
            assert_eq!(desktop.titles(), ["Keep me", "Phone message"]);
        }
    }

    #[tokio::test]
    async fn updates_replace_in_place_and_removals_close() {
        let desktop = FakeDesktop::default();
        let mut mirror = NotificationMirror::default();

        apply(
            &mut mirror,
            &desktop,
            vec![posted(7, "Ana"), posted(7, "Ana (2)")],
        )
        .await;
        assert_eq!(desktop.calls(), ["show 1", "show 1"]);
        assert_eq!(desktop.titles(), ["Ana (2)"]);

        apply(
            &mut mirror,
            &desktop,
            vec![NotificationEvent::Removed { id: 7 }],
        )
        .await;
        assert_eq!(desktop.calls(), ["close 1"]);
        assert!(desktop.titles().is_empty());
    }

    #[tokio::test]
    async fn an_identical_repost_shows_nothing_new() {
        let desktop = FakeDesktop::default();
        let mut mirror = NotificationMirror::default();
        apply(
            &mut mirror,
            &desktop,
            vec![posted(1, "Ana"), posted(1, "Ana")],
        )
        .await;
        assert_eq!(desktop.calls(), ["show 1"]);
    }

    #[tokio::test]
    async fn a_snapshot_reconciles_after_a_reconnect() {
        let desktop = FakeDesktop::default();
        let mut mirror = NotificationMirror::default();
        apply(
            &mut mirror,
            &desktop,
            vec![
                posted(1, "kept"),
                posted(2, "changed"),
                posted(3, "dismissed"),
            ],
        )
        .await;
        desktop.calls();

        // While disconnected: 2 changed, 3 was dismissed, 4 arrived.
        apply(
            &mut mirror,
            &desktop,
            vec![
                NotificationEvent::SnapshotStart,
                posted(1, "kept"),
                posted(2, "changed again"),
                posted(4, "new"),
                NotificationEvent::SnapshotEnd,
            ],
        )
        .await;

        assert_eq!(desktop.calls(), ["show 2", "show 4", "close 3"]);
        assert_eq!(desktop.titles(), ["changed again", "kept", "new"]);
    }

    #[tokio::test]
    async fn close_all_clears_the_phone_from_the_desktop() {
        let desktop = FakeDesktop::default();
        let mut mirror = NotificationMirror::default();
        apply(&mut mirror, &desktop, vec![posted(1, "a"), posted(2, "b")]).await;
        mirror.close_all(&desktop).await;
        assert!(desktop.titles().is_empty());
    }

    #[test]
    fn text_is_escaped_truncated_and_labelled() {
        let shown = desktop_notification(
            &PhoneNotification {
                id: 1,
                app_name: "Mail".into(),
                title: "<b>Boss</b> & co".into(),
                body: "é".repeat(PhoneNotification::MAX_BODY_BYTES),
                icon_png: Some(vec![0; PhoneNotification::MAX_ICON_BYTES + 1]),
                posted_ms: 0,
            },
            "Pixel",
        );
        assert_eq!(shown.app_name, "Mail (Pixel)");
        assert_eq!(shown.summary, "&lt;b&gt;Boss&lt;/b&gt; &amp; co");
        assert!(shown.body.len() <= PhoneNotification::MAX_BODY_BYTES && shown.body.ends_with('…'));
        assert_eq!(shown.icon_png, None, "oversized icons are dropped");
    }

    #[test]
    fn an_untitled_notification_uses_the_app_name() {
        let shown = desktop_notification(
            &PhoneNotification {
                id: 1,
                app_name: "Clock".into(),
                title: String::new(),
                body: "Alarm".into(),
                icon_png: None,
                posted_ms: 0,
            },
            "Pixel",
        );
        assert_eq!(shown.summary, "Clock");
    }
}
