//! The UI's link to the edge-kvm backend over its local control endpoint.
//!
//! One task keeps a connection open, reconnecting after the backend
//! restarts. Each reconnect starts from a fresh snapshot; nothing is
//! replayed. The pages see the connection state and the latest snapshot as
//! events, and act only through the explicit commands in `commands`.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use edge_control::{
    client::{Client, ConnectError, RequestError},
    protocol::{ControlError, ErrorKind, Event, Hello, Reply, Request, Snapshot},
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;

const RETRY: Duration = Duration::from_secs(1);
/// After a mismatched instance, check less often.
const SLOW_RETRY: Duration = Duration::from_secs(3);

/// What the pages show about the backend itself.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Connection {
    Connecting,
    /// Nothing is serving the endpoint.
    Absent {
        detail: String,
    },
    /// We asked it to start and are waiting for it.
    Starting,
    Incompatible {
        server: u32,
        ours: u32,
    },
    /// A backend is running, but for different settings than this UI was
    /// started for (another portable folder).
    OtherInstance {
        config_path: PathBuf,
        expected: PathBuf,
    },
    Connected {
        hello: Box<Hello>,
    },
    /// Was connected; reconnecting.
    Disconnected {
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct UiState {
    pub connection: Connection,
    pub snapshot: Option<Box<Snapshot>>,
    /// The settings file to edit while the backend is down, if known.
    pub offline_config: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<edge_control::FieldError>,
}

impl UiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            fields: Vec::new(),
        }
    }
}

impl From<ControlError> for UiError {
    fn from(error: ControlError) -> Self {
        Self {
            kind: error.kind,
            message: error.message,
            fields: error.fields,
        }
    }
}

impl From<RequestError> for UiError {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Control(error) => error.into(),
            other => Self::new(ErrorKind::Unavailable, other.to_string()),
        }
    }
}

pub struct Backend {
    app: AppHandle,
    client: tokio::sync::RwLock<Option<Arc<Client>>>,
    state: Mutex<UiState>,
    /// Wakes the reconnect loop early, after asking the backend to start.
    wake: Notify,
    /// Set when this UI was started for one particular config file.
    expected_config: Option<PathBuf>,
}

impl Backend {
    pub fn new(app: AppHandle, expected_config: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            app,
            client: tokio::sync::RwLock::new(None),
            state: Mutex::new(UiState {
                connection: Connection::Connecting,
                snapshot: None,
                offline_config: expected_config.clone(),
            }),
            wake: Notify::new(),
            expected_config,
        })
    }

    pub fn state(&self) -> UiState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn hello(&self) -> Option<Hello> {
        match &self.state().connection {
            Connection::Connected { hello } => Some((**hello).clone()),
            _ => None,
        }
    }

    pub fn offline_config(&self) -> Option<PathBuf> {
        self.state().offline_config
    }

    fn set_connection(&self, connection: Connection) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Connection::Connected { hello } = &connection {
            state.offline_config = Some(hello.config_path.clone());
        }
        if !matches!(connection, Connection::Connected { .. }) {
            state.snapshot = None;
        }
        state.connection = connection.clone();
        drop(state);
        let _ = self.app.emit("backend://connection", &connection);
    }

    fn set_snapshot(&self, snapshot: Box<Snapshot>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Snapshots can't arrive out of order on one connection, but a
        // reconnect resets nothing here, so guard anyway.
        if state
            .snapshot
            .as_ref()
            .is_some_and(|current| current.revision > snapshot.revision)
        {
            return;
        }
        state.snapshot = Some(snapshot.clone());
        drop(state);
        let _ = self.app.emit("backend://snapshot", &snapshot);
    }

    pub fn mark_starting(&self) {
        self.set_connection(Connection::Starting);
        self.wake.notify_one();
    }

    /// Sends one request to the connected backend.
    pub async fn request(&self, request: Request) -> Result<Reply, UiError> {
        let client = self.client.read().await.clone().ok_or_else(|| {
            UiError::new(ErrorKind::Unavailable, "hyprKvm's backend isn't connected")
        })?;
        Ok(client.request(request).await?)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state().connection, Connection::Connected { .. })
    }

    /// Runs for the life of the app.
    pub async fn run(self: Arc<Self>) {
        loop {
            let delay = match Client::connect().await {
                Ok((hello, client, mut events)) => {
                    if let Some(expected) = &self.expected_config
                        && !same_file(expected, &hello.config_path)
                    {
                        self.set_connection(Connection::OtherInstance {
                            config_path: hello.config_path.clone(),
                            expected: expected.clone(),
                        });
                        SLOW_RETRY
                    } else {
                        let client = Arc::new(client);
                        match client.request(Request::Subscribe).await {
                            Ok(Reply::Snapshot(snapshot)) => {
                                *self.client.write().await = Some(client.clone());
                                self.set_connection(Connection::Connected {
                                    hello: Box::new(hello),
                                });
                                self.set_snapshot(snapshot);
                                while let Some(event) = events.recv().await {
                                    self.handle_event(event);
                                }
                                *self.client.write().await = None;
                                self.set_connection(Connection::Disconnected {
                                    detail: "the backend closed the connection".into(),
                                });
                            }
                            Ok(other) => self.set_connection(Connection::Disconnected {
                                detail: format!("unexpected reply to subscribe: {other:?}"),
                            }),
                            Err(error) => self.set_connection(Connection::Disconnected {
                                detail: error.to_string(),
                            }),
                        }
                        RETRY
                    }
                }
                Err(ConnectError::Absent(detail)) => {
                    // Keep showing "starting" while we wait for it.
                    if !matches!(self.state().connection, Connection::Starting) {
                        self.set_connection(Connection::Absent { detail });
                    }
                    RETRY
                }
                Err(ConnectError::Incompatible { server }) => {
                    self.set_connection(Connection::Incompatible {
                        server,
                        ours: edge_control::protocol::PROTOCOL_VERSION,
                    });
                    SLOW_RETRY
                }
                Err(ConnectError::NoAnswer(detail)) => {
                    self.set_connection(Connection::Disconnected { detail });
                    RETRY
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    fn handle_event(&self, event: Event) {
        match event {
            Event::Snapshot(snapshot) => self.set_snapshot(snapshot),
            Event::PhonePairing(update) => {
                let _ = self.app.emit("backend://phone-pairing", &update);
            }
            Event::Stopping { restarting } => {
                let _ = self.app.emit("backend://stopping", restarting);
            }
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a) == canonical(b)
}
