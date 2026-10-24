//! Local phone projections share the hub's keys, even while its bridge is absent.
//! The private cache contains daemon-scoped IDs and digests, never notification text.
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use edge_protocol::{NotificationKey, SharedNotification};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::notifications::{Desktop, FreedesktopNotifications};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct PhoneProjections {
    owner: String,
    entries: BTreeMap<String, Entry>,
    #[serde(skip)]
    loaded: bool,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    id: u32,
    digest: String,
    dismissed: bool,
}
impl PhoneProjections {
    pub async fn load(&mut self, desktop: &FreedesktopNotifications, path: &Path) -> Result<()> {
        let owner = desktop.generation().await?;
        if !self.loaded {
            match tokio::fs::metadata(path).await {
                Ok(meta) => {
                    ensure!(meta.len() <= 2 * 1024 * 1024, "phone projection cache full");
                    let saved: Self = serde_json::from_slice(&tokio::fs::read(path).await?)?;
                    if saved.owner == owner {
                        *self = saved;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            self.loaded = true;
        }
        if self.owner != owner {
            self.owner = owner;
            self.entries.clear();
        }
        Ok(())
    }
    pub fn instance(&self, prefix: &str) -> Option<String> {
        self.entries
            .keys()
            .find(|key| key.starts_with(prefix))
            .cloned()
    }
    pub fn dismissed(&self, instance: &str) -> bool {
        self.entries
            .get(instance)
            .is_some_and(|entry| entry.dismissed)
    }
    async fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "phone projection cache full"
        );
        let temporary = path.with_extension("pending");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(temporary, path).await?;
        let parent = path
            .parent()
            .context("phone projection cache directory")?
            .to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
        Ok(())
    }
    pub async fn show(
        &mut self,
        desktop: &FreedesktopNotifications,
        path: &Path,
        key: &NotificationKey,
        content: Option<&SharedNotification>,
        fresh: bool,
    ) -> Result<()> {
        self.load(desktop, path).await?;
        let Some(content) = content else {
            if let Some(entry) = self.entries.get_mut(&key.instance)
                && !entry.dismissed
            {
                desktop.close(entry.id, &self.owner).await?;
                entry.dismissed = true;
                self.save(path).await?;
            }
            return Ok(());
        };
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(content)?));
        let replaces = match self.entries.get(&key.instance) {
            Some(entry) if entry.dismissed || entry.digest == digest => return Ok(()),
            Some(entry) => entry.id,
            None => 0,
        };
        ensure!(
            self.entries.contains_key(&key.instance) || self.entries.len() < 4096,
            "phone projection cache full"
        );
        let id = desktop
            .show_shared(key, content, replaces, "", !fresh)
            .await?;
        self.entries.insert(
            key.instance.clone(),
            Entry {
                id,
                digest,
                dismissed: false,
            },
        );
        self.save(path).await
    }
    pub async fn adopt(
        &mut self,
        desktop: &FreedesktopNotifications,
        path: &Path,
        key: &NotificationKey,
        id: u32,
        content: &SharedNotification,
    ) -> Result<()> {
        self.load(desktop, path).await?;
        if let Some(entry) = self.entries.get(&key.instance) {
            if entry.id == id {
                return Ok(());
            }
            // Prefer the retained shell entry and retire an older duplicate.
            desktop.close(entry.id, &self.owner).await?;
        }
        ensure!(
            self.entries.contains_key(&key.instance) || self.entries.len() < 4096,
            "phone projection cache full"
        );
        self.entries.insert(
            key.instance.clone(),
            Entry {
                id,
                digest: hex::encode(Sha256::digest(serde_json::to_vec(content)?)),
                dismissed: false,
            },
        );
        self.save(path).await
    }
    pub async fn closed(
        &mut self,
        path: &Path,
        instance: &str,
        id: u32,
        reason: u32,
    ) -> Result<bool> {
        let Some(entry) = self
            .entries
            .get_mut(instance)
            .filter(|entry| entry.id == id)
        else {
            return Ok(false);
        };
        if reason == 2 {
            entry.dismissed = true;
        } else {
            self.entries.remove(instance);
        }
        self.save(path).await?;
        Ok(true)
    }
    pub async fn withdraw(
        &mut self,
        desktop: &FreedesktopNotifications,
        path: &Path,
        instance: &str,
    ) -> Result<()> {
        self.load(desktop, path).await?;
        if let Some(entry) = self.entries.remove(instance) {
            if !entry.dismissed {
                desktop.close(entry.id, &self.owner).await?;
            }
            self.save(path).await?;
        }
        Ok(())
    }
    pub fn missing(
        &self,
        prefix: &str,
        present: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        self.entries
            .keys()
            .filter(|key| {
                key.starts_with(prefix)
                    && !present.iter().any(|id| key.starts_with(&format!("{id}/")))
            })
            .cloned()
            .collect()
    }
}
