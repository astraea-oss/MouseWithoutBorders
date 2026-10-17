//! One durable notification writer, replicas, and a private Quickshell bridge.
//! The hub is independent of input roles and never owns a notification UI.
use crate::{AbortOnDropTask, notifications::FreedesktopNotifications};
use anyhow::{Context, Result, bail, ensure};
use edge_common::NotificationConfig;
use edge_protocol::{
    NotificationKey, NotificationOperation, NotificationRecord, NotificationSync,
    SharedNotification,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, mpsc, watch},
};

const MAX_RECORDS: usize = 4096;
const MAX_PENDING: usize = 1024;
const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;
const MAX_BRIDGE_LINE: u64 = 16384;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Store {
    coordinator: String,
    epoch: String,
    local_identity: String,
    sequence: u64,
    records: Vec<NotificationRecord>,
    pending: Vec<NotificationOperation>,
}
impl Store {
    fn coordinator_locked(&self) -> bool {
        !self.epoch.is_empty()
            || !self.records.is_empty()
            || self
                .pending
                .iter()
                .any(|op| op.key.origin != self.local_identity)
    }
    fn apply(&mut self, sender: &str, operation: &NotificationOperation) -> Result<bool> {
        validate_key(&operation.key)?;
        ensure!(
            operation.sequence > 0,
            "zero notification operation sequence"
        );
        if let Some(content) = &operation.content {
            ensure!(
                operation.key.origin == sender,
                "notification source impersonation"
            );
            validate_content(content)?;
        }
        if let Some(record) = self.records.iter_mut().find(|r| r.key == operation.key) {
            // Removal is terminal even if the source reconnects with stale state.
            if record.content.is_none() {
                return Ok(false);
            }
            if operation.content.is_none() || operation.sequence > record.source_sequence {
                record.content = operation.content.clone();
                if operation.content.is_some() {
                    record.source_sequence = operation.sequence;
                }
            } else {
                return Ok(false);
            }
        } else {
            ensure!(
                self.records.len() < MAX_RECORDS,
                "notification state is full; explicit reset required"
            );
            self.records.push(NotificationRecord {
                key: operation.key.clone(),
                source_sequence: operation.sequence,
                content: operation.content.clone(),
            });
        }
        Ok(true)
    }
    fn view(&self) -> Vec<NotificationRecord> {
        let mut overlay = self.clone();
        for operation in &self.pending {
            // Pending operations have already been validated before persistence.
            let _ = overlay.apply(&self.local_identity, operation);
        }
        overlay.records
    }
}
fn validate_key(key: &NotificationKey) -> Result<()> {
    ensure!(
        key.origin.len() == 35
            && key.origin.split(':').count() == 4
            && key
                .origin
                .split(':')
                .all(|part| part.len() == 8 && part.bytes().all(|c| c.is_ascii_hexdigit())),
        "invalid notification origin"
    );
    ensure!(
        !key.instance.is_empty()
            && key.instance.len() <= 160
            && key
                .instance
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_/".contains(&c)),
        "invalid notification instance"
    );
    Ok(())
}
fn validate_content(content: &SharedNotification) -> Result<()> {
    ensure!(
        content.app_name.chars().count() <= 64
            && content.title.chars().count() <= 256
            && content.body.len() <= 4096,
        "notification content too large"
    );
    ensure!(
        content.icon.len() <= 128
            && content
                .icon
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "notification icon must be a theme name"
    );
    ensure!(content.urgency <= 2, "invalid notification urgency");
    Ok(())
}

fn new_epoch() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("notification epoch generation: {e}"))?;
    Ok(hex::encode(bytes))
}
pub async fn coordinator_locked_on_disk(path: &Path) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            ensure!(
                meta.len() <= MAX_STORE_BYTES as u64,
                "notification store too large"
            );
            Ok(
                serde_json::from_slice::<Store>(&tokio::fs::read(path).await?)?
                    .coordinator_locked(),
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone)]
pub struct NotificationHub(Arc<Inner>);
struct Inner {
    config: NotificationConfig,
    identity: String,
    name: String,
    path: PathBuf,
    state: Mutex<Store>,
    changed: watch::Sender<u64>,
    bridges: Arc<AtomicUsize>,
    links: Arc<AtomicUsize>,
    error: std::sync::Mutex<Option<String>>,
    bridge_error: std::sync::Mutex<Option<String>>,
}
pub struct NotificationService {
    pub hub: NotificationHub,
    _bridge: AbortOnDropTask,
}
impl NotificationHub {
    pub async fn start(
        config: &NotificationConfig,
        identity: &str,
        name: &str,
        directory: &Path,
    ) -> Result<Option<NotificationService>> {
        if !config.enabled {
            return Ok(None);
        }
        validate_key(&NotificationKey {
            origin: config.coordinator_fingerprint.clone(),
            instance: "config".into(),
        })?;
        for peer in &config.peers {
            validate_key(&NotificationKey {
                origin: peer.clone(),
                instance: "config".into(),
            })?;
        }
        ensure!(
            config.coordinator_fingerprint == identity
                || config.peers.contains(&config.coordinator_fingerprint),
            "notification coordinator must be an enrolled peer"
        );
        tokio::fs::create_dir_all(directory).await?;
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        let path = directory.join("store.json");
        let store = match tokio::fs::metadata(&path).await {
            Ok(meta) => {
                ensure!(
                    meta.len() <= MAX_STORE_BYTES as u64,
                    "notification store too large"
                );
                let mut state: Store = serde_json::from_slice(&tokio::fs::read(&path).await?)
                    .context("notification store corrupt; explicit recovery required")?;
                ensure!(
                    state.local_identity == identity,
                    "notification identity changed; explicit migration required"
                );
                if state.coordinator != config.coordinator_fingerprint {
                    ensure!(
                        !state.coordinator_locked(),
                        "notification history owner changed; explicit migration required"
                    );
                    let backup = directory.join(format!(
                        "store.before-owner-change-{}.json",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_nanos()
                    ));
                    tokio::fs::copy(&path, backup).await?;
                    state.coordinator = config.coordinator_fingerprint.clone();
                    if state.coordinator == identity {
                        // A bridge may have queued local notifications between
                        // choosing the owner and restarting. Commit them here;
                        // initial setup must never discard that local outbox.
                        for operation in std::mem::take(&mut state.pending) {
                            state.apply(identity, &operation)?;
                        }
                        state.epoch = new_epoch()?;
                    }
                }
                ensure!(
                    state.records.len() <= MAX_RECORDS && state.pending.len() <= MAX_PENDING,
                    "notification store limits exceeded"
                );
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Store {
                epoch: if config.coordinator_fingerprint == identity {
                    new_epoch()?
                } else {
                    String::new()
                },
                coordinator: config.coordinator_fingerprint.clone(),
                local_identity: identity.into(),
                ..Store::default()
            },
            Err(error) => return Err(error.into()),
        };
        let socket = directory.join("bridge.sock");
        if let Ok(metadata) = tokio::fs::symlink_metadata(&socket).await {
            ensure!(
                metadata.file_type().is_socket(),
                "notification endpoint is not a socket"
            );
            ensure!(
                UnixStream::connect(&socket).await.is_err(),
                "notification bridge is already running"
            );
            tokio::fs::remove_file(&socket).await?;
        }
        let listener = UnixListener::bind(&socket)
            .context("bind notification bridge (Unix socket paths must fit the OS limit)")?;
        tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).await?;
        let (changed, _) = watch::channel(0);
        let hub = Self(Arc::new(Inner {
            config: config.clone(),
            identity: identity.into(),
            name: name.into(),
            path,
            state: Mutex::new(store),
            changed,
            bridges: Arc::default(),
            links: Arc::default(),
            error: std::sync::Mutex::new(None),
            bridge_error: std::sync::Mutex::new(None),
        }));
        hub.persist(&*hub.0.state.lock().await).await?;
        let worker = hub.clone();
        let task = tokio::spawn(async move {
            let uid = match std::fs::metadata("/proc/self") {
                Ok(m) => m.uid(),
                Err(_) => return,
            };
            while let Ok((stream, _)) = listener.accept().await {
                if stream.peer_cred().map(|c| c.uid() != uid).unwrap_or(true) {
                    continue;
                }
                // Exactly one shell bridge. It can reconnect after EOF; bounded
                // writes prevent a stalled client from monopolizing the endpoint.
                if let Err(error) = worker.bridge(stream).await {
                    if let Ok(mut stored) = worker.0.bridge_error.lock() {
                        *stored = Some(error.to_string());
                    }
                    tracing::warn!(%error, "notification bridge disconnected");
                }
            }
        });
        tracing::info!(path = %socket.display(), coordinator = hub.is_coordinator(), "notification sync enabled");
        Ok(Some(NotificationService {
            hub,
            _bridge: AbortOnDropTask(task),
        }))
    }
    pub fn bridge_connected(&self) -> bool {
        self.0.bridges.load(Ordering::Relaxed) > 0
    }
    pub async fn coordinator_locked(&self) -> bool {
        self.0.state.lock().await.coordinator_locked()
    }
    pub async fn status(&self) -> String {
        if let Some(error) = self.0.error.lock().ok().and_then(|e| e.clone()) {
            return format!("Needs attention: {error}");
        }
        if let Some(error) = self.0.bridge_error.lock().ok().and_then(|e| e.clone()) {
            return format!("Quickshell needs attention: {error}");
        }
        if self.0.bridges.load(Ordering::Relaxed) == 0 {
            return "Waiting for Quickshell bridge — install or reload the integration".into();
        }
        if self.0.links.load(Ordering::Relaxed) == 0 {
            return "Quickshell connected; waiting for the other PC to enable/update sharing and confirm the history owner"
                .into();
        }
        let state = self.0.state.lock().await;
        if !self.is_coordinator() && state.epoch.is_empty() {
            return format!(
                "Owner confirmed; waiting for initial history · {} queued",
                state.pending.len()
            );
        }
        let retained = state.view().iter().filter(|r| r.content.is_some()).count();
        format!(
            "Connected · {retained} notifications · {} queued",
            state.pending.len()
        )
    }
    fn record_error(&self, error: &str) {
        if let Ok(mut stored) = self.0.error.lock()
            && stored.is_none()
        {
            *stored = Some(error.to_owned());
        }
    }
    fn clear_error(&self) {
        if let Ok(mut stored) = self.0.error.lock() {
            *stored = None;
        }
    }
    fn is_coordinator(&self) -> bool {
        self.0.identity == self.0.config.coordinator_fingerprint
    }
    fn enrolled(&self, peer: &str) -> bool {
        self.0.config.peers.iter().any(|p| p == peer)
    }
    async fn persist(&self, state: &Store) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        ensure!(
            bytes.len() <= MAX_STORE_BYTES,
            "notification store capacity exceeded"
        );
        let temporary = self.0.path.with_extension("pending");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temporary, &self.0.path).await?;
        // Persist the directory entry before acknowledging a committed change.
        let parent = self
            .0
            .path
            .parent()
            .context("notification store directory")?
            .to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
        Ok(())
    }
    fn publish(&self) {
        self.0
            .changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    async fn local(&self, key: NotificationKey, content: Option<SharedNotification>) -> Result<()> {
        let mut state = self.0.state.lock().await;
        // Repeated inventory, updates, and retries must not allocate new operations.
        if let Some(record) = state.view().iter().find(|r| r.key == key)
            && (record.content.is_none() || record.content == content)
        {
            return Ok(());
        }
        let mut next = state.clone();
        next.sequence = next
            .sequence
            .checked_add(1)
            .context("notification sequence exhausted")?;
        let operation = NotificationOperation {
            sequence: next.sequence,
            key,
            content,
        };
        validate_key(&operation.key)?;
        if let Some(content) = &operation.content {
            validate_content(content)?;
            ensure!(
                operation.key.origin == self.0.identity,
                "invalid local origin"
            );
        }
        if self.is_coordinator() {
            next.apply(&self.0.identity, &operation)?;
        } else {
            // Keep only the newest unsent operation for an instance. A terminal
            // removal cannot be overwritten because the view check above rejects it.
            next.pending.retain(|pending| pending.key != operation.key);
            ensure!(
                next.pending.len() < MAX_PENDING
                    && next.records.len() + next.pending.len() < MAX_RECORDS,
                "notification outbox is full"
            );
            next.pending.push(operation);
        }
        self.persist(&next).await?;
        *state = next;
        drop(state);
        self.publish();
        Ok(())
    }
    pub fn link(&self, peer: &str, supported: bool) -> Option<NotificationLink> {
        if !supported
            || !self.enrolled(peer)
            || (!self.is_coordinator() && peer != self.0.config.coordinator_fingerprint)
        {
            return None;
        }
        let mut changes = self.0.changed.subscribe();
        changes.mark_changed();
        self.clear_error();
        Some(NotificationLink {
            _activity: None,
            sent_through: 0,
            hub: self.clone(),
            peer: peer.into(),
            changes,
            outgoing: VecDeque::from([NotificationSync::Settings {
                coordinator: self.0.config.coordinator_fingerprint.clone(),
            }]),
            staged: None,
            dirty: false,
            pacing: tokio::time::interval(std::time::Duration::from_millis(2)),
        })
    }
    async fn bridge(&self, stream: UnixStream) -> Result<()> {
        let desktop =
            FreedesktopNotifications::connect(self.0.path.with_file_name("icons")).await?;
        if let Ok(mut stored) = self.0.bridge_error.lock() {
            *stored = None;
        }
        let _activity = Activity::new(self.0.bridges.clone());
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut changes = self.0.changed.subscribe();
        let mut native: BTreeMap<NotificationKey, u32> = BTreeMap::new();
        let mut projections: BTreeMap<NotificationKey, (u32, SharedNotification)> = BTreeMap::new();
        let mut inventory: Option<Vec<NotificationKey>> = None;
        let mut line = Vec::new();
        // Wait for the shell's complete inventory before creating any copies.
        loop {
            let mut limited =
                (&mut reader).take(MAX_BRIDGE_LINE.saturating_sub(line.len() as u64) + 1);
            tokio::select! {
                read = limited.read_until(b'\n', &mut line) => {
                    if read? == 0 { return Ok(()); }
                    ensure!(line.len() <= MAX_BRIDGE_LINE as usize && line.ends_with(b"\n"), "notification bridge line too large");
                    let message: BridgeMessage = serde_json::from_slice(&line)?;
                    line.clear();
                    match message {
                        BridgeMessage::Start => { native.clear(); projections.clear(); inventory = Some(Vec::new()); }
                        BridgeMessage::Posted { id, instance, content } => {
                            let key = NotificationKey { origin: self.0.identity.clone(), instance };
                            native.insert(key.clone(), id);
                            if let Some(keys) = &mut inventory { keys.push(key.clone()); }
                            self.local(key, Some(content)).await?;
                        }
                        BridgeMessage::Copy { id, key, content } => {
                            // Only a known committed entry can be registered as a copy.
                            if self.0.state.lock().await.view().iter().any(|r| r.key == key && r.content.is_some()) {
                                projections.insert(key, (id, content));
                            }
                        }
                        BridgeMessage::Closed { id, reason, instance, key } => {
                            if let Some(key) = native.iter().find(|(_, value)| **value == id).map(|(k, _)| k.clone()) {
                                native.remove(&key);
                                if (1..=3).contains(&reason) { self.local(key, None).await?; }
                            } else if let Some(key) = projections.iter().find(|(_, (value, _))| *value == id).map(|(k, _)| k.clone()) {
                                projections.remove(&key);
                                if reason == 2 { self.local(key, None).await?; }
                            } else if (1..=3).contains(&reason) {
                                // Recover a closure queued while the local backend was down.
                                let candidate = key.or_else(|| instance.map(|instance| NotificationKey { origin: self.0.identity.clone(), instance }));
                                if let Some(key) = candidate {
                                    let known = self.0.state.lock().await.view().iter().any(|r| r.key == key);
                                    if known && (key.origin == self.0.identity || reason == 2) { self.local(key, None).await?; }
                                }
                            }
                        }
                        BridgeMessage::End => {
                            ensure!(inventory.take().is_some(), "notification inventory end without start");
                            // Absence after a shell restart is not a user dismissal.
                            // Restore cached originals as silent projections instead.
                        }
                    }
                    ensure!(native.len() + projections.len() <= MAX_RECORDS, "notification bridge inventory full");
                }
                changed = changes.changed(), if inventory.is_none() => { changed?; }
            }
            if inventory.is_some() {
                continue;
            }
            let records = self.0.state.lock().await.view();
            for record in &records {
                match &record.content {
                    None => {
                        if let Some(id) = native.remove(&record.key) {
                            // Remove mapping first: the resulting close callback is not user intent.
                            desktop.close_shared(id).await.ok();
                        }
                        if let Some((id, _)) = projections.remove(&record.key) {
                            desktop.close_shared(id).await.ok();
                        }
                    }
                    Some(content) if !native.contains_key(&record.key) => {
                        let previous = projections.get(&record.key);
                        if previous.is_some_and(|(_, old)| old == content) {
                            continue;
                        }
                        let replaces = previous.map_or(0, |(id, _)| *id);
                        // Every reconciliation is silent. Fresh source posts receive a popup
                        // only on their originating device in the first milestone.
                        let id = desktop
                            .show_shared(&record.key, content, replaces, &self.0.name)
                            .await?;
                        projections.insert(record.key.clone(), (id, content.clone()));
                    }
                    _ => {}
                }
            }
            // Full coordinator snapshots may remove old projection keys.
            let absent: Vec<_> = projections
                .keys()
                .filter(|key| {
                    !records
                        .iter()
                        .any(|r| &r.key == *key && r.content.is_some())
                })
                .cloned()
                .collect();
            for key in absent {
                if let Some((id, _)) = projections.remove(&key) {
                    desktop.close_shared(id).await.ok();
                }
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                writer.write_all(b"{\"type\":\"ready\"}\n"),
            )
            .await??;
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum BridgeMessage {
    Start,
    Posted {
        id: u32,
        instance: String,
        content: SharedNotification,
    },
    Copy {
        id: u32,
        key: NotificationKey,
        content: SharedNotification,
    },
    Closed {
        id: u32,
        reason: u32,
        #[serde(default)]
        instance: Option<String>,
        #[serde(default)]
        key: Option<NotificationKey>,
    },
    End,
}

struct Activity(Arc<AtomicUsize>);
impl Activity {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct NotificationLink {
    _activity: Option<Activity>,
    sent_through: u64,
    hub: NotificationHub,
    peer: String,
    changes: watch::Receiver<u64>,
    outgoing: VecDeque<NotificationSync>,
    staged: Option<(String, Vec<NotificationRecord>)>,
    dirty: bool,
    pacing: tokio::time::Interval,
}
impl NotificationLink {
    pub async fn next(&mut self) -> NotificationSync {
        self.pacing.tick().await;
        loop {
            if let Some(event) = self.outgoing.pop_front() {
                return event;
            }
            // The session worker cancels this wait when the peer's Settings
            // arrives. Do not emit records or local submissions before agreement.
            if self._activity.is_none() {
                std::future::pending::<()>().await;
            }
            if !self.dirty {
                if self.changes.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                self.dirty = true;
            }
            let state = self.hub.0.state.lock().await;
            self.dirty = false;
            if self.hub.is_coordinator() {
                self.outgoing.push_back(NotificationSync::SnapshotStart {
                    epoch: state.epoch.clone(),
                });
                self.outgoing
                    .extend(state.records.iter().cloned().map(NotificationSync::Record));
                self.outgoing.push_back(NotificationSync::SnapshotEnd);
            } else {
                for operation in &state.pending {
                    if operation.sequence > self.sent_through {
                        self.outgoing
                            .push_back(NotificationSync::Submit(operation.clone()));
                    }
                }
                if let Some(latest) = state.pending.iter().map(|op| op.sequence).max() {
                    self.sent_through = self.sent_through.max(latest);
                }
            }
        }
    }
    pub async fn receive(&mut self, event: NotificationSync) -> Result<()> {
        if let NotificationSync::Settings { coordinator } = event {
            ensure!(
                coordinator == self.hub.0.config.coordinator_fingerprint,
                "History owner differs between PCs. Choose the same computer in Notifications settings on both PCs"
            );
            if self._activity.is_none() {
                self._activity = Some(Activity::new(self.hub.0.links.clone()));
            }
            return Ok(());
        }
        ensure!(
            self._activity.is_some(),
            "Notification peer did not confirm the history owner"
        );
        match event {
            NotificationSync::Submit(operation) if self.hub.is_coordinator() => {
                let mut state = self.hub.0.state.lock().await;
                let mut next = state.clone();
                let changed = next.apply(&self.peer, &operation)?;
                if changed {
                    self.hub.persist(&next).await?;
                    *state = next;
                }
                drop(state);
                if changed {
                    self.hub.publish();
                }
                ensure!(
                    self.outgoing.len() < MAX_RECORDS + MAX_PENDING + 4,
                    "notification output queue full"
                );
                self.outgoing.push_back(NotificationSync::Ack {
                    sequence: operation.sequence,
                });
            }
            NotificationSync::Ack { sequence } if !self.hub.is_coordinator() => {
                let mut state = self.hub.0.state.lock().await;
                if !state.pending.iter().any(|op| op.sequence == sequence) {
                    return Ok(());
                }
                let mut next = state.clone();
                next.pending.retain(|op| op.sequence != sequence);
                // Keep an acknowledged optimistic operation until the next full
                // snapshot containing it arrives; the coordinator snapshots before Ack.
                if let Some(op) = state.pending.iter().find(|op| op.sequence == sequence) {
                    next.apply(&self.hub.0.identity, op)?;
                }
                self.hub.persist(&next).await?;
                *state = next;
                drop(state);
                self.hub.publish();
            }
            NotificationSync::SnapshotStart { epoch } if !self.hub.is_coordinator() => {
                ensure!(
                    epoch.len() == 32 && epoch.bytes().all(|c| c.is_ascii_hexdigit()),
                    "invalid coordinator store epoch"
                );
                let state = self.hub.0.state.lock().await;
                ensure!(
                    state.epoch.is_empty() || state.epoch == epoch,
                    "coordinator store reset; explicit recovery required"
                );
                ensure!(self.staged.is_none(), "nested notification snapshot");
                self.staged = Some((epoch, Vec::new()));
            }
            NotificationSync::Record(record) if !self.hub.is_coordinator() => {
                validate_key(&record.key)?;
                if let Some(content) = &record.content {
                    validate_content(content)?;
                }
                let (_, staged) = self
                    .staged
                    .as_mut()
                    .context("notification record outside snapshot")?;
                ensure!(
                    staged.len() < MAX_RECORDS && !staged.iter().any(|r| r.key == record.key),
                    "notification snapshot capacity/duplicate key"
                );
                staged.push(record);
            }
            NotificationSync::SnapshotEnd if !self.hub.is_coordinator() => {
                let (epoch, records) = self
                    .staged
                    .take()
                    .context("notification snapshot end without start")?;
                let mut state = self.hub.0.state.lock().await;
                if state.epoch == epoch && state.records == records {
                    return Ok(());
                }
                let mut next = state.clone();
                next.epoch = epoch;
                next.records = records;
                self.hub.persist(&next).await?;
                *state = next;
                drop(state);
                self.hub.publish();
            }
            _ => bail!("notification event from incorrect authority"),
        }
        Ok(())
    }
}

struct ByteBudget {
    shared: Arc<AtomicUsize>,
    bytes: usize,
}
impl ByteBudget {
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.shared
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_STORE_BYTES)
            })
            .map_err(|_| {
                anyhow::anyhow!("Notification snapshot buffer is full; reconnect to retry")
            })?;
        self.bytes += bytes;
        Ok(())
    }
}
impl Drop for ByteBudget {
    fn drop(&mut self) {
        self.shared.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}
struct IncomingSnapshot {
    epoch: String,
    records: Vec<NotificationRecord>,
    budget: ByteBudget,
}
enum IncomingNotification {
    Event(NotificationSync),
    Snapshot(IncomingSnapshot),
}

/// Snapshot frames are staged with bounded memory and constant work per frame.
/// Validation and disk writes stay in the worker; KVM never waits for storage.
pub struct SessionLink {
    hub: NotificationHub,
    outgoing: mpsc::Receiver<NotificationSync>,
    incoming: mpsc::Sender<IncomingNotification>,
    staged: Option<IncomingSnapshot>,
    buffered_bytes: Arc<AtomicUsize>,
    _worker: AbortOnDropTask,
}
impl SessionLink {
    fn enqueue(&mut self, event: NotificationSync) -> Result<()> {
        let request = match event {
            NotificationSync::SnapshotStart { epoch } => {
                ensure!(self.staged.is_none(), "nested notification snapshot");
                ensure!(epoch.len() == 32, "invalid notification snapshot epoch");
                self.staged = Some(IncomingSnapshot {
                    epoch,
                    records: Vec::new(),
                    budget: ByteBudget {
                        shared: self.buffered_bytes.clone(),
                        bytes: 0,
                    },
                });
                return Ok(());
            }
            NotificationSync::Record(record) => {
                let snapshot = self
                    .staged
                    .as_mut()
                    .context("notification record outside snapshot")?;
                ensure!(
                    snapshot.records.len() < MAX_RECORDS,
                    "notification snapshot too large"
                );
                let bytes = record.key.origin.len()
                    + record.key.instance.len()
                    + 256
                    + record.content.as_ref().map_or(0, |c| {
                        c.app_name.len() + c.title.len() + c.body.len() + c.icon.len()
                    });
                snapshot.budget.reserve(bytes)?;
                snapshot.records.push(record);
                return Ok(());
            }
            NotificationSync::SnapshotEnd => IncomingNotification::Snapshot(
                self.staged
                    .take()
                    .context("notification snapshot end without start")?,
            ),
            event => {
                ensure!(
                    self.staged.is_none(),
                    "notification control interrupted a snapshot"
                );
                IncomingNotification::Event(event)
            }
        };
        self.incoming
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow::anyhow!("Notification worker queue is full; reconnect to retry")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow::anyhow!("Notification worker stopped; reconnect to retry")
                }
            })
    }
    pub fn spawn(mut link: NotificationLink) -> Self {
        let (incoming, mut requests) = mpsc::channel(32);
        let (replies, outgoing) = mpsc::channel(32);
        let hub = link.hub.clone();
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    event = requests.recv() => {
                        let Some(event) = event else { break; };
                        let received = async {
                            match event {
                                IncomingNotification::Event(event) => link.receive(event).await,
                                IncomingNotification::Snapshot(snapshot) => {
                                    link.receive(NotificationSync::SnapshotStart { epoch: snapshot.epoch }).await?;
                                    for record in snapshot.records { link.receive(NotificationSync::Record(record)).await?; }
                                    // Keep the budget until the snapshot has committed.
                                    let result = link.receive(NotificationSync::SnapshotEnd).await;
                                    drop(snapshot.budget);
                                    result
                                }
                            }
                        }.await;
                        if let Err(error) = received {
                            link.hub.record_error(&error.to_string());
                            tracing::warn!(%error, "notification sync worker stopped");
                            break;
                        }
                    }
                    event = link.next() => {
                        if replies.send(event).await.is_err() { break; }
                    }
                }
            }
        });
        Self {
            hub,
            incoming,
            outgoing,
            staged: None,
            buffered_bytes: Arc::default(),
            _worker: AbortOnDropTask(worker),
        }
    }
}
pub async fn next(link: &mut Option<SessionLink>) -> NotificationSync {
    if let Some(session) = link {
        if let Some(event) = session.outgoing.recv().await {
            return event;
        }
        session
            .hub
            .record_error("Notification session stopped; reconnect to retry");
        tracing::warn!("notification worker ended; notifications disabled for session");
        *link = None;
    }
    std::future::pending().await
}
pub async fn receive(link: &mut Option<SessionLink>, event: NotificationSync) {
    if let Some(session) = link
        && let Err(error) = session.enqueue(event)
    {
        session.hub.record_error(&error.to_string());
        tracing::warn!(%error, "notification reception stopped; notifications disabled for session");
        *link = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn snapshots_larger_than_the_worker_queue_survive_a_burst_while_storage_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let service = NotificationHub::start(
            &NotificationConfig {
                enabled: true,
                coordinator_fingerprint: a.clone(),
                peers: vec![a.clone()],
            },
            &b,
            "Lua",
            dir.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let mut session = Some(SessionLink::spawn(service.hub.link(&a, true).unwrap()));
        let budget = session.as_ref().unwrap().buffered_bytes.clone();
        let busy = service.hub.0.state.lock().await;
        receive(
            &mut session,
            NotificationSync::Settings {
                coordinator: a.clone(),
            },
        )
        .await;
        receive(
            &mut session,
            NotificationSync::SnapshotStart {
                epoch: "1".repeat(32),
            },
        )
        .await;
        for i in 0..96 {
            receive(
                &mut session,
                NotificationSync::Record(NotificationRecord {
                    key: NotificationKey {
                        origin: a.clone(),
                        instance: format!("burst/{i}"),
                    },
                    source_sequence: i + 1,
                    content: Some(content()),
                }),
            )
            .await;
            assert!(
                session.is_some(),
                "a valid snapshot overflowed the transport at record {i}"
            );
        }
        receive(&mut session, NotificationSync::SnapshotEnd).await;
        assert!(session.is_some());
        assert!(budget.load(Ordering::Relaxed) > 0);
        drop(busy);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if service.hub.0.state.lock().await.records.len() == 96
                    && budget.load(Ordering::Relaxed) == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(service.hub.0.error.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn snapshot_and_ack_changes_do_not_resubmit_operations_already_sent_on_this_link() {
        let dir = tempfile::tempdir().unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let service = NotificationHub::start(
            &NotificationConfig {
                enabled: true,
                coordinator_fingerprint: a.clone(),
                peers: vec![a.clone()],
            },
            &b,
            "Lua",
            dir.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let key = NotificationKey {
            origin: b,
            instance: "local/1".into(),
        };
        service
            .hub
            .local(key.clone(), Some(content()))
            .await
            .unwrap();
        let mut link = service.hub.link(&a, true).unwrap();
        assert!(matches!(
            link.next().await,
            NotificationSync::Settings { .. }
        ));
        link.receive(NotificationSync::Settings {
            coordinator: a.clone(),
        })
        .await
        .unwrap();
        let NotificationSync::Submit(first) = link.next().await else {
            panic!("missing first operation")
        };
        service.hub.publish();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), link.next())
                .await
                .is_err()
        );
        let mut update = content();
        update.title = "Updated".into();
        service.hub.local(key, Some(update)).await.unwrap();
        let NotificationSync::Submit(second) = link.next().await else {
            panic!("missing new operation")
        };
        assert!(second.sequence > first.sequence);
        let mut reconnect = service.hub.link(&a, true).unwrap();
        reconnect.next().await;
        reconnect
            .receive(NotificationSync::Settings { coordinator: a })
            .await
            .unwrap();
        assert!(
            matches!(reconnect.next().await, NotificationSync::Submit(op) if op.sequence == second.sequence)
        );
    }

    #[tokio::test]
    async fn mismatched_owners_are_rejected_before_any_history_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let service = NotificationHub::start(
            &NotificationConfig {
                enabled: true,
                coordinator_fingerprint: b.clone(),
                peers: vec![b.clone()],
            },
            &a,
            "Lua",
            dir.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let mut link = service.hub.link(&b, true).unwrap();
        assert!(matches!(
            link.next().await,
            NotificationSync::Settings { .. }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), link.next())
                .await
                .is_err()
        );
        assert!(
            link.receive(NotificationSync::Settings { coordinator: a })
                .await
                .unwrap_err()
                .to_string()
                .contains("History owner differs")
        );
        assert_eq!(service.hub.0.links.load(Ordering::Relaxed), 0);
        assert!(!service.hub.coordinator_locked().await);
    }

    #[tokio::test]
    async fn an_unsynchronized_replica_can_correct_its_owner_without_losing_local_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let mut config = NotificationConfig {
            enabled: true,
            coordinator_fingerprint: a.clone(),
            peers: vec![a],
        };
        let replica = NotificationHub::start(&config, &b, "Tera", dir.path())
            .await
            .unwrap()
            .unwrap();
        assert!(!replica.hub.coordinator_locked().await);
        replica
            .hub
            .local(
                NotificationKey {
                    origin: b.clone(),
                    instance: "setup/1".into(),
                },
                Some(content()),
            )
            .await
            .unwrap();
        assert!(!replica.hub.coordinator_locked().await);
        drop(replica);
        tokio::task::yield_now().await;
        config.coordinator_fingerprint = b.clone();
        let corrected = NotificationHub::start(&config, &b, "Tera", dir.path())
            .await
            .unwrap()
            .unwrap();
        assert!(corrected.hub.is_coordinator());
        assert!(corrected.hub.coordinator_locked().await);
        assert_eq!(corrected.hub.0.state.lock().await.records.len(), 1);
        assert!(corrected.hub.0.state.lock().await.pending.is_empty());
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten().any(|p| {
            p.file_name()
                .to_string_lossy()
                .starts_with("store.before-owner-change-")
        }));
        let mut state = Store::default();
        state.pending.push(NotificationOperation {
            sequence: 1,
            key: key(),
            content: Some(content()),
        });
        assert!(state.coordinator_locked());
    }

    #[tokio::test]
    async fn settings_status_follows_connections_and_keeps_the_original_error() {
        let directory = tempfile::tempdir().unwrap();
        let identity = edge_crypto::fingerprint(&[1; 32]);
        let peer = edge_crypto::fingerprint(&[2; 32]);
        let service = NotificationHub::start(
            &NotificationConfig {
                enabled: true,
                coordinator_fingerprint: identity.clone(),
                peers: vec![peer.clone()],
            },
            &identity,
            "Main",
            directory.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let hub = &service.hub;
        assert!(hub.status().await.contains("Waiting for Quickshell"));
        let bridge = Activity::new(hub.0.bridges.clone());
        assert!(hub.status().await.contains("waiting for the other PC"));
        let mut link = hub.link(&peer, true).unwrap();
        link.receive(NotificationSync::Settings {
            coordinator: identity.clone(),
        })
        .await
        .unwrap();
        assert!(hub.status().await.starts_with("Connected"));
        hub.record_error("coordinator store reset; explicit recovery required");
        hub.record_error("Notification session stopped");
        assert!(hub.status().await.contains("coordinator store reset"));
        drop(link);
        let mut link = hub.link(&peer, true).unwrap();
        link.receive(NotificationSync::Settings {
            coordinator: identity.clone(),
        })
        .await
        .unwrap();
        assert!(hub.status().await.starts_with("Connected"));
        drop(link);
        drop(bridge);
        assert!(hub.status().await.contains("Waiting for Quickshell"));
    }

    fn key() -> NotificationKey {
        NotificationKey {
            origin: edge_crypto::fingerprint(&[1; 32]),
            instance: "epoch-1/42".into(),
        }
    }
    fn content() -> SharedNotification {
        SharedNotification {
            app_name: "Mail".into(),
            title: "Hello".into(),
            body: "world".into(),
            icon: "mail-unread".into(),
            posted_ms: 1,
            urgency: 1,
        }
    }
    #[test]
    fn dismissal_is_terminal_and_stale_updates_cannot_replace_newer_content() {
        let mut store = Store::default();
        let op = |sequence, content| NotificationOperation {
            key: key(),
            sequence,
            content,
        };
        store.apply(&key().origin, &op(3, Some(content()))).unwrap();
        let mut old = content();
        old.title = "Old".into();
        store.apply(&key().origin, &op(2, Some(old))).unwrap();
        assert_eq!(store.records[0].content, Some(content()));
        store
            .apply(&edge_crypto::fingerprint(&[2; 32]), &op(1, None))
            .unwrap();
        store.apply(&key().origin, &op(4, Some(content()))).unwrap();
        assert!(store.records[0].content.is_none());
        assert_eq!(store.records.len(), 1);
    }
    #[test]
    fn source_impersonation_and_remote_paths_are_rejected() {
        let mut store = Store::default();
        assert!(
            store
                .apply(
                    &edge_crypto::fingerprint(&[2; 32]),
                    &NotificationOperation {
                        sequence: 1,
                        key: key(),
                        content: Some(content())
                    }
                )
                .is_err()
        );
        let mut invalid = content();
        invalid.icon = "/etc/passwd".into();
        assert!(validate_content(&invalid).is_err());
    }
    #[test]
    fn pending_dismissal_overrides_reconnect_snapshot() {
        let mut store = Store {
            local_identity: edge_crypto::fingerprint(&[2; 32]),
            ..Store::default()
        };
        store
            .apply(
                &key().origin,
                &NotificationOperation {
                    sequence: 1,
                    key: key(),
                    content: Some(content()),
                },
            )
            .unwrap();
        store.pending.push(NotificationOperation {
            sequence: 2,
            key: key(),
            content: None,
        });
        assert!(store.view()[0].content.is_none());
        let recovered: Store =
            serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        assert!(recovered.view()[0].content.is_none());
    }
    #[tokio::test]
    async fn two_nodes_reconcile_offline_dismissal_and_reject_a_reset_authority() {
        let directory = tempfile::tempdir().unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let main_config = NotificationConfig {
            enabled: true,
            coordinator_fingerprint: a.clone(),
            peers: vec![b.clone()],
        };
        let laptop_config = NotificationConfig {
            enabled: true,
            coordinator_fingerprint: a.clone(),
            peers: vec![a.clone()],
        };
        let main = NotificationHub::start(&main_config, &a, "Main", &directory.path().join("main"))
            .await
            .unwrap()
            .unwrap();
        let laptop = NotificationHub::start(
            &laptop_config,
            &b,
            "Laptop",
            &directory.path().join("laptop"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            main.hub
                .link(&edge_crypto::fingerprint(&[3; 32]), true)
                .is_none()
        );
        assert!(main.hub.link(&b, false).is_none());
        main.hub.local(key(), Some(content())).await.unwrap();
        let mut outgoing = main.hub.link(&b, true).unwrap();
        let mut incoming = laptop.hub.link(&a, true).unwrap();
        incoming.receive(outgoing.next().await).await.unwrap();
        outgoing.receive(incoming.next().await).await.unwrap();
        loop {
            let event = outgoing.next().await;
            let end = matches!(event, NotificationSync::SnapshotEnd);
            incoming.receive(event).await.unwrap();
            if end {
                break;
            }
        }
        assert_eq!(
            laptop.hub.0.state.lock().await.view()[0].content,
            Some(content())
        );
        // The laptop can dismiss while disconnected; persist before returning.
        laptop.hub.local(key(), None).await.unwrap();
        let restored: Store = serde_json::from_slice(
            &tokio::fs::read(directory.path().join("laptop/store.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(restored.view()[0].content.is_none());
        assert_eq!(restored.pending.len(), 1);
        outgoing.receive(incoming.next().await).await.unwrap();
        for _ in 0..4 {
            let event = outgoing.next().await;
            incoming.receive(event).await.unwrap();
        }
        assert!(laptop.hub.0.state.lock().await.pending.is_empty());
        assert!(main.hub.0.state.lock().await.records[0].content.is_none());
        assert!(laptop.hub.0.state.lock().await.view()[0].content.is_none());
        // A lost/recreated coordinator store must not silently clear replicas.
        assert!(
            incoming
                .receive(NotificationSync::SnapshotStart {
                    epoch: "0".repeat(32)
                })
                .await
                .is_err()
        );
        // Delayed source inventories cannot resurrect the dismissed instance.
        main.hub.local(key(), Some(content())).await.unwrap();
        assert!(main.hub.0.state.lock().await.records[0].content.is_none());
        // Reloading durable state restores the terminal marker.
        let recovered: Store = serde_json::from_slice(
            &tokio::fs::read(directory.path().join("main/store.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(recovered.records[0].content.is_none());
    }
    /// Run only on a private bus; this test owns its notification service name.
    #[tokio::test]
    #[ignore = "requires dbus-run-session, Quickshell 0.3.2, and EDGE_KVM_NOTIFICATION_TEST=1"]
    async fn quickshell_bridge_roundtrip() {
        assert_eq!(
            std::env::var("EDGE_KVM_NOTIFICATION_TEST").as_deref(),
            Ok("1")
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations/quickshell");
        let original = tokio::fs::read_to_string(assets.join("tests/shell.qml"))
            .await
            .unwrap();
        let start = original.find("    EdgeNotificationBridge {").unwrap();
        let end = start + original[start..].find("\n    }\n").unwrap() + "\n    }\n".len();
        let original = format!("{}{}", &original[..start], &original[end..])
            .replace("        property string epoch: \"\"\n", "")
            .replace("        property string offlineClosures: \"[]\"\n", "")
            .replace(
                "Number(notification.hints[\"x-edge-kvm-posted-ms\"]) || Date.now()",
                "Date.now()",
            )
            .replace(
                "if (root.dnd || notification.hints[\"x-edge-kvm-replay\"]) return;",
                "if (root.dnd) return;",
            );
        tokio::fs::create_dir(root.join("checkout")).await.unwrap();
        tokio::fs::write(root.join("checkout/shell.qml"), original)
            .await
            .unwrap();
        std::os::unix::fs::symlink(root.join("checkout/shell.qml"), root.join("shell.qml"))
            .unwrap();
        let a = edge_crypto::fingerprint(&[1; 32]);
        let b = edge_crypto::fingerprint(&[2; 32]);
        let service = NotificationHub::start(
            &NotificationConfig {
                enabled: true,
                coordinator_fingerprint: a.clone(),
                peers: vec![b.clone()],
            },
            &a,
            "Main",
            &root.join("state"),
        )
        .await
        .unwrap()
        .unwrap();
        let mut environment = BTreeMap::new();
        for (key, value) in [
            ("QT_QPA_PLATFORM", "offscreen"),
            ("QT_QPA_PLATFORMTHEME", "generic"),
            ("QT_QUICK_CONTROLS_STYLE", "Basic"),
            ("QT_STYLE_OVERRIDE", "Fusion"),
            ("QS_NO_RELOAD_POPUP", "1"),
        ] {
            environment.insert(key.to_string(), value.to_string());
        }
        for (key, directory) in [
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "qs-state"),
        ] {
            let path = root.join(directory);
            tokio::fs::create_dir(&path).await.unwrap();
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .await
                .unwrap();
            environment.insert(key.to_string(), path.display().to_string());
        }
        environment.insert(
            "EDGE_KVM_NOTIFICATION_SOCKET".into(),
            root.join("state/bridge.sock").display().to_string(),
        );
        let log = std::fs::File::create(root.join("shell.log")).unwrap();
        let mut child = tokio::process::Command::new("quickshell")
            .args(["-p"])
            .arg(root)
            .envs(&environment)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        async fn entries(
            root: &Path,
            env: &BTreeMap<String, String>,
        ) -> Option<Vec<serde_json::Value>> {
            let output = tokio::process::Command::new("quickshell")
                .arg("-p")
                .arg(root)
                .args(["ipc", "--any-display", "call", "test", "entries"])
                .envs(env)
                .output()
                .await
                .ok()?;
            serde_json::from_slice(&output.stdout).ok()
        }
        for _ in 0..100 {
            if entries(root, &environment).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            entries(root, &environment).await.is_some(),
            "{}",
            std::fs::read_to_string(root.join("shell.log")).unwrap()
        );
        let connection = zbus::Connection::session().await.unwrap();
        async fn notify(connection: &zbus::Connection, replaces: u32, title: &str) -> u32 {
            connection
                .call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &(
                        "Local test",
                        replaces,
                        "",
                        title,
                        "body",
                        Vec::<&str>::new(),
                        std::collections::HashMap::<&str, zbus::zvariant::Value>::new(),
                        0_i32,
                    ),
                )
                .await
                .unwrap()
                .body()
                .deserialize()
                .unwrap()
        }
        let native_id = notify(&connection, 0, "Original").await;
        assert!(service.hub.0.state.lock().await.records.is_empty());
        crate::notification_setup::install_bridge(
            &root.join("shell.qml"),
            &root.join("state/bridge.sock"),
        )
        .unwrap();
        for _ in 0..150 {
            if service.hub.0.bridges.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            service.hub.0.bridges.load(Ordering::Relaxed) > 0,
            "Hot-installed bridge did not connect with retained history: {}",
            std::fs::read_to_string(root.join("shell.log")).unwrap()
        );
        for _ in 0..100 {
            if service.hub.0.state.lock().await.records.len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            service.hub.0.state.lock().await.records.len(),
            1,
            "native notification was not ingested"
        );
        assert_eq!(notify(&connection, native_id, "Updated").await, native_id);
        for _ in 0..100 {
            if service.hub.0.state.lock().await.records[0]
                .content
                .as_ref()
                .unwrap()
                .title
                == "Updated"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            service.hub.0.state.lock().await.records[0]
                .content
                .as_ref()
                .unwrap()
                .title,
            "Updated"
        );
        let remote = NotificationKey {
            origin: b.clone(),
            instance: "remote/1".into(),
        };
        let mut link = service.hub.link(&b, true).unwrap();
        link.receive(NotificationSync::Settings {
            coordinator: a.clone(),
        })
        .await
        .unwrap();
        link.receive(NotificationSync::Submit(NotificationOperation {
            sequence: 1,
            key: remote.clone(),
            content: Some(content()),
        }))
        .await
        .unwrap();
        let mut remote_id = None;
        for _ in 0..100 {
            let list = entries(root, &environment).await.unwrap();
            remote_id = list
                .iter()
                .find(|e| e["key"].as_str().is_some_and(|key| !key.is_empty()))
                .map(|e| e["id"].as_u64().unwrap());
            if remote_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let remote_id = remote_id.expect("remote notification not projected");
        let output = tokio::process::Command::new("quickshell")
            .arg("-p")
            .arg(root)
            .args([
                "ipc",
                "--any-display",
                "call",
                "test",
                "dismiss",
                &remote_id.to_string(),
            ])
            .envs(&environment)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        for _ in 0..100 {
            if service
                .hub
                .0
                .state
                .lock()
                .await
                .records
                .iter()
                .any(|r| r.key == remote && r.content.is_none())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            service
                .hub
                .0
                .state
                .lock()
                .await
                .records
                .iter()
                .any(|r| r.key == remote && r.content.is_none()),
            "dismissal did not reach hub"
        );
        // Reload must preserve source identity and must not create a duplicate.
        let original_instance = service
            .hub
            .0
            .state
            .lock()
            .await
            .records
            .iter()
            .find(|r| r.key.origin == a)
            .unwrap()
            .key
            .clone();
        let mut shell = tokio::fs::OpenOptions::new()
            .append(true)
            .open(root.join("shell.qml"))
            .await
            .unwrap();
        shell.write_all(b"\n// integration reload\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(entries(root, &environment).await.unwrap().len(), 1);
        let state = service.hub.0.state.lock().await;
        assert_eq!(state.records.len(), 2);
        assert!(
            state
                .records
                .iter()
                .any(|r| r.key == original_instance && r.content.is_some())
        );
        drop(state);
        // Phone snapshots must also remain idempotent across a backend restart
        // and a Quickshell reload, without entering the shared desktop set.
        use crate::notifications::{Desktop, NotificationMirror};
        let desktop = FreedesktopNotifications::connect(root.join("phone-icons"))
            .await
            .unwrap();
        let generation = desktop.generation().await.unwrap();
        let phone = edge_protocol::PhoneNotification {
            id: 42,
            app_name: "Phone app".into(),
            title: "Retained phone entry".into(),
            body: "body".into(),
            icon_png: None,
            posted_ms: 1,
        };
        let path = root.join("phone-cache/identities.json");
        let mut mirror = NotificationMirror::default();
        mirror.restore(&desktop, &path).await.unwrap();
        mirror
            .apply(
                edge_protocol::NotificationEvent::Posted(phone.clone()),
                &desktop,
                "Test phone",
            )
            .await
            .unwrap();
        assert_eq!(entries(root, &environment).await.unwrap().len(), 2);
        drop(mirror);
        let mut mirror = NotificationMirror::default();
        mirror.restore(&desktop, &path).await.unwrap();
        mirror
            .apply(
                edge_protocol::NotificationEvent::SnapshotStart,
                &desktop,
                "Test phone",
            )
            .await
            .unwrap();
        mirror
            .apply(
                edge_protocol::NotificationEvent::Posted(phone.clone()),
                &desktop,
                "Test phone",
            )
            .await
            .unwrap();
        mirror
            .apply(
                edge_protocol::NotificationEvent::SnapshotEnd,
                &desktop,
                "Test phone",
            )
            .await
            .unwrap();
        assert_eq!(entries(root, &environment).await.unwrap().len(), 2);
        shell
            .write_all(b"\n// phone identity reload\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(desktop.generation().await.unwrap(), generation);
        mirror
            .apply(
                edge_protocol::NotificationEvent::Posted(phone),
                &desktop,
                "Test phone",
            )
            .await
            .unwrap();
        assert_eq!(entries(root, &environment).await.unwrap().len(), 2);
        assert_eq!(service.hub.0.state.lock().await.records.len(), 2);
        child.kill().await.unwrap();
    }
}
