//! The framed control protocol (`edge_control::protocol`) on the receiver's
//! control socket, for the hyprKvm desktop UI.
//!
//! The text commands in `control` keep working on the same socket: a framed
//! client starts with a NUL byte, which no text command does.
//!
//! The receiver stays the only owner of sessions, pins and the config file.
//! Every request maps onto something the tray menu, the phone sessions or the
//! settings window could already do; nothing here edits pins directly.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use edge_common::AppConfig;
use edge_control::{
    SettingsDraft,
    framing::{FrameError, read_frame_async, supports, write_frame_async},
    protocol::{
        Capabilities, ClientMessage, ControlError, Controller, ErrorKind, Event, Hello, LinkState,
        Outcome, PROTOCOL_VERSION, PhonePairingOffer, PhonePairingUpdate, PhoneState, Reply,
        Request, ServerMessage, SettingsView, Snapshot,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    sync::{mpsc, oneshot, watch},
    time,
};

use crate::{
    append_portable_log,
    notification_setup::NotificationControl,
    phone::{PairingOffer, PhoneRequest},
    tray::{ControllerChoice, ReceiverTrayHandle, TrayCommand, TrayStatus},
};

/// Requests a client may queue before the server stops reading from it.
const REQUEST_QUEUE: usize = 16;
const PHONE_LIST_TIMEOUT: Duration = Duration::from_millis(500);

static REVISION: AtomicU64 = AtomicU64::new(1);

/// What the server needs from the running receiver.
#[derive(Clone)]
pub(crate) struct ControlContext {
    pub requests: mpsc::UnboundedSender<PhoneRequest>,
    pub tray: Option<ReceiverTrayHandle>,
    pub notifications: Option<NotificationControl>,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
}

/// Serves one framed client. `reader` has already consumed the NUL byte.
pub(crate) async fn serve(
    mut reader: impl AsyncRead + Send + Unpin + 'static,
    mut writer: impl AsyncWrite + Unpin,
    context: ControlContext,
) -> Result<(), FrameError> {
    let mut rest = [0; 7];
    reader.read_exact(&mut rest).await?;
    let version = edge_control::framing::parse_preamble_rest(rest)?;
    if !supports(version) {
        write_frame_async(
            &mut writer,
            &ServerMessage::Incompatible {
                server_protocol: PROTOCOL_VERSION,
            },
        )
        .await?;
        return Ok(());
    }
    write_frame_async(&mut writer, &ServerMessage::Hello(hello(&context))).await?;

    // Reading happens on its own task so a half-read frame is never lost to
    // a select! branch being cancelled.
    let (incoming_tx, mut incoming) = mpsc::channel(REQUEST_QUEUE);
    let read_task = tokio::spawn(async move {
        loop {
            let message = read_frame_async::<ClientMessage>(&mut reader).await;
            let fatal = matches!(
                message,
                Err(FrameError::Closed | FrameError::TooLarge(_) | FrameError::Io(_))
            );
            if incoming_tx.send(message).await.is_err() || fatal {
                break;
            }
        }
    });
    let _read_task = crate::AbortOnDropTask(read_task);

    let mut connection = Connection {
        context,
        status: None,
        pairing: None,
    };
    let result = loop {
        tokio::select! {
            message = incoming.recv() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(FrameError::Malformed(error))) => {
                        // The frame boundary is intact, so keep the connection.
                        let outcome = Outcome::Error(ControlError::new(
                            ErrorKind::BadRequest,
                            format!("unreadable request: {error}"),
                        ));
                        write_frame_async(&mut writer, &ServerMessage::Response { id: 0, outcome }).await?;
                        continue;
                    }
                    Some(Err(error)) => break Err(error),
                    None => break Ok(()),
                };
                let (outcome, after) = connection.handle(message.request).await;
                write_frame_async(&mut writer, &ServerMessage::Response { id: message.id, outcome }).await?;
                match after {
                    After::Nothing => {}
                    After::PushSnapshot => {
                        if connection.status.is_some() {
                            let event = Event::Snapshot(Box::new(connection.snapshot().await));
                            write_frame_async(&mut writer, &ServerMessage::Event { event }).await?;
                        }
                    }
                    After::Restart => {
                        let event = Event::Stopping { restarting: true };
                        write_frame_async(&mut writer, &ServerMessage::Event { event }).await.ok();
                        restart_receiver(&connection.context);
                    }
                }
            }
            changed = changed(&mut connection.status) => {
                if changed.is_err() {
                    // The tray is gone: the receiver is shutting down.
                    let event = Event::Stopping { restarting: false };
                    write_frame_async(&mut writer, &ServerMessage::Event { event }).await.ok();
                    break Ok(());
                }
                let event = Event::Snapshot(Box::new(connection.snapshot().await));
                write_frame_async(&mut writer, &ServerMessage::Event { event }).await?;
            }
            outcome = paired(&mut connection.pairing) => {
                connection.pairing = None;
                let update = match outcome {
                    Some(name) => {
                        append_portable_log(&connection.context.log_path, format!("hyprKvm paired phone {name}"));
                        PhonePairingUpdate::Paired { name }
                    }
                    None => PhonePairingUpdate::Closed,
                };
                let event = Event::PhonePairing(update);
                write_frame_async(&mut writer, &ServerMessage::Event { event }).await?;
                if connection.status.is_some() {
                    let event = Event::Snapshot(Box::new(connection.snapshot().await));
                    write_frame_async(&mut writer, &ServerMessage::Event { event }).await?;
                }
            }
        }
    };
    connection.close_pairing();
    result
}

fn hello(context: &ControlContext) -> Hello {
    let state_dir = edge_common::default_state_dir();
    Hello {
        protocol: PROTOCOL_VERSION,
        backend_version: env!("CARGO_PKG_VERSION").to_string(),
        platform: "linux".to_string(),
        pid: std::process::id(),
        executable: std::env::current_exe().unwrap_or_default(),
        config_path: absolute(&context.config_path),
        log_path: absolute(&context.log_path),
        state_dir: absolute(&state_dir),
        service_managed: std::env::var_os("EDGE_KVM_SERVICE_MANAGED").is_some(),
    }
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Waits for the tray status to change, or forever when not subscribed.
async fn changed(
    status: &mut Option<watch::Receiver<TrayStatus>>,
) -> Result<(), watch::error::RecvError> {
    match status {
        Some(status) => status.changed().await,
        None => std::future::pending().await,
    }
}

struct PendingPairing {
    paired: oneshot::Receiver<String>,
    deadline: time::Instant,
}

/// The paired phone's name, or `None` once the window closes or expires.
async fn paired(pairing: &mut Option<PendingPairing>) -> Option<String> {
    match pairing {
        Some(pending) => time::timeout_at(pending.deadline, &mut pending.paired)
            .await
            .ok()
            .and_then(Result::ok),
        None => std::future::pending().await,
    }
}

enum After {
    Nothing,
    PushSnapshot,
    Restart,
}

struct Connection {
    context: ControlContext,
    status: Option<watch::Receiver<TrayStatus>>,
    pairing: Option<PendingPairing>,
}

fn error(kind: ErrorKind, message: impl Into<String>) -> Outcome {
    Outcome::Error(ControlError::new(kind, message))
}

fn done() -> (Outcome, After) {
    (Outcome::Ok(Reply::Done), After::Nothing)
}

impl Connection {
    fn tray(&self) -> Result<&ReceiverTrayHandle, Outcome> {
        self.context.tray.as_ref().ok_or_else(|| {
            error(
                ErrorKind::Unsupported,
                "this needs the tray; restart the receiver without --no-tray",
            )
        })
    }

    fn current(&self) -> Option<TrayStatus> {
        self.context
            .tray
            .as_ref()
            .map(|tray| tray.status().borrow().clone())
    }

    async fn handle(&mut self, request: Request) -> (Outcome, After) {
        match self.try_handle(request).await {
            Ok(result) => result,
            Err(outcome) => (outcome, After::Nothing),
        }
    }

    async fn try_handle(&mut self, request: Request) -> Result<(Outcome, After), Outcome> {
        Ok(match request {
            Request::Subscribe => {
                if let Some(tray) = &self.context.tray {
                    let mut status = tray.status();
                    status.mark_unchanged();
                    self.status = Some(status);
                }
                (snapshot_reply(self.snapshot().await), After::Nothing)
            }
            Request::Snapshot => (snapshot_reply(self.snapshot().await), After::Nothing),
            Request::SetInputForwarding { enabled } => {
                let tray = self.tray()?;
                let snapshot = &tray.status().borrow().snapshot.clone();
                if snapshot.state != LinkState::Connected {
                    return Err(error(ErrorKind::Unavailable, "no computer is connected"));
                }
                // The session loop only knows how to toggle.
                if snapshot.input_forwarding != enabled {
                    tray.command(TrayCommand::ToggleInputForwarding);
                }
                done()
            }
            Request::SetController { controller } => {
                let tray = self.tray()?;
                let snapshot = tray.status().borrow().snapshot.clone();
                if !snapshot.can_switch_controller {
                    return Err(error(
                        ErrorKind::Unavailable,
                        "control can't be handed over right now",
                    ));
                }
                if snapshot.switching_controller {
                    return Err(error(ErrorKind::Busy, "control is already changing hands"));
                }
                tray.command(TrayCommand::SetController(match controller {
                    Controller::Local => ControllerChoice::Local,
                    Controller::Peer => ControllerChoice::Peer,
                }));
                done()
            }
            Request::SetAudio { target } => {
                self.tray()?.choose_audio(target).await;
                done()
            }
            Request::Disconnect => {
                self.tray()?.command(TrayCommand::Disconnect);
                done()
            }
            Request::Reconnect => {
                self.tray()?.command(TrayCommand::Reconnect);
                done()
            }
            Request::ArmComputerPairing => {
                self.tray()?.command(TrayCommand::ArmPairing);
                done()
            }
            Request::StartPhonePairing => {
                // Replacing our own earlier window is fine; the phone side
                // keeps only the newest token anyway.
                self.close_pairing();
                let (reply, offer) = oneshot::channel();
                let (paired_tx, paired) = oneshot::channel();
                self.context
                    .requests
                    .send(PhoneRequest::OpenPairing {
                        reply,
                        paired: paired_tx,
                    })
                    .map_err(|_| error(ErrorKind::Unavailable, "phone sessions aren't running"))?;
                let PairingOffer { uri, expires_in } = offer
                    .await
                    .map_err(|_| error(ErrorKind::Failed, "phone pairing didn't open"))?;
                self.pairing = Some(PendingPairing {
                    paired,
                    deadline: time::Instant::now() + expires_in,
                });
                (
                    Outcome::Ok(Reply::PhonePairing(PhonePairingOffer {
                        uri,
                        expires_in_secs: expires_in.as_secs(),
                    })),
                    After::Nothing,
                )
            }
            Request::CancelPhonePairing => {
                self.close_pairing();
                done()
            }
            Request::UnpairPhone { name } => {
                let (reply, result) = oneshot::channel();
                self.context
                    .requests
                    .send(PhoneRequest::Unpair {
                        name: name.clone(),
                        reply,
                    })
                    .map_err(|_| error(ErrorKind::Unavailable, "phone sessions aren't running"))?;
                match result.await {
                    Ok(Ok(true)) => (Outcome::Ok(Reply::Done), After::PushSnapshot),
                    Ok(Ok(false)) => {
                        return Err(error(ErrorKind::NotFound, format!("{name} isn't paired")));
                    }
                    Ok(Err(message)) => return Err(error(ErrorKind::Failed, message)),
                    Err(_) => {
                        return Err(error(ErrorKind::Failed, "no answer from phone sessions"));
                    }
                }
            }
            Request::Settings => {
                let config = self.load_config().await?;
                (
                    Outcome::Ok(Reply::Settings(Box::new(SettingsView {
                        draft: SettingsDraft::from_config(&config),
                        config_path: absolute(&self.context.config_path),
                        connector_only: false,
                        local_address: edge_common::detect_primary_local_ip()
                            .map(|ip| ip.to_string()),
                        audio_buffers_ms: edge_control::AUDIO_BUFFERS_MS.to_vec(),
                    }))),
                    After::Nothing,
                )
            }
            Request::ValidateSettings { draft } => {
                let config = self.load_config().await?;
                let setup = self.notification_setup().await;
                let errors = draft.validate(false, &config.notifications, setup.as_ref());
                (Outcome::Ok(Reply::Validation { errors }), After::Nothing)
            }
            Request::SaveSettings { draft, base } => {
                let config = self.load_config().await?;
                if SettingsDraft::from_config(&config) != base {
                    return Err(error(
                        ErrorKind::Conflict,
                        "the settings changed since this page loaded; reload and try again",
                    ));
                }
                let setup = self.notification_setup().await;
                let errors = draft.validate(false, &config.notifications, setup.as_ref());
                if !errors.is_empty() {
                    return Err(Outcome::Error(ControlError::invalid(errors)));
                }
                let updated = draft
                    .apply(&config, false)
                    .map_err(|e| error(ErrorKind::Invalid, format!("{e:#}")))?;
                updated
                    .save(&self.context.config_path)
                    .await
                    .map_err(|e| error(ErrorKind::Failed, format!("couldn't save: {e}")))?;
                append_portable_log(
                    &self.context.log_path,
                    "settings saved from hyprKvm; restarting node",
                );
                (
                    Outcome::Ok(Reply::Saved { restarting: true }),
                    After::Restart,
                )
            }
            Request::Autostart => {
                let enabled = tokio::task::spawn_blocking(crate::control_link::autostart)
                    .await
                    .ok()
                    .flatten();
                (Outcome::Ok(Reply::Autostart { enabled }), After::Nothing)
            }
            Request::SetAutostart { enabled } => {
                tokio::task::spawn_blocking(move || crate::control_link::set_autostart(enabled))
                    .await
                    .map_err(|e| error(ErrorKind::Failed, e.to_string()))?
                    .map_err(|e| error(ErrorKind::Failed, format!("{e:#}")))?;
                done()
            }
            Request::Notifications => {
                let setup = self.notification_setup().await;
                (Outcome::Ok(Reply::Notifications { setup }), After::Nothing)
            }
            Request::InstallNotificationBridge { shell_path } => {
                let setup = self.notification_setup().await.ok_or_else(|| {
                    error(ErrorKind::Unavailable, "notification sharing isn't running")
                })?;
                let text = tokio::task::spawn_blocking(move || {
                    crate::notification_setup::install_bridge(&shell_path, &setup.socket_path)
                })
                .await
                .map_err(|e| error(ErrorKind::Failed, e.to_string()))?
                .map_err(|e| error(ErrorKind::Failed, format!("{e:#}")))?;
                (Outcome::Ok(Reply::Message { text }), After::Nothing)
            }
            Request::Diagnostics => {
                let report = self.report().await;
                (Outcome::Ok(Reply::Diagnostics { report }), After::Nothing)
            }
            Request::Quit => {
                append_portable_log(&self.context.log_path, "quit requested from hyprKvm");
                match &self.context.tray {
                    Some(tray) => tray.command(TrayCommand::Quit),
                    None => unsafe {
                        libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
                    },
                }
                done()
            }
        })
    }

    async fn load_config(&self) -> Result<AppConfig, Outcome> {
        AppConfig::load(&self.context.config_path)
            .await
            .map_err(|e| {
                error(
                    ErrorKind::Failed,
                    format!("couldn't read the settings: {e}"),
                )
            })
    }

    async fn notification_setup(&self) -> Option<edge_control::NotificationSetup> {
        match &self.context.notifications {
            Some(notifications) => notifications.setup().await.ok(),
            None => None,
        }
    }

    fn close_pairing(&mut self) {
        if self.pairing.take().is_some() {
            // Dropping our receiver first marks the window as unwatched.
            self.context.requests.send(PhoneRequest::ClosePairing).ok();
        }
    }

    async fn snapshot(&self) -> Snapshot {
        let mut snapshot = match self.current() {
            Some(status) => status.snapshot,
            None => headless_snapshot(),
        };
        snapshot.revision = REVISION.fetch_add(1, Ordering::Relaxed);
        snapshot.capabilities.notifications = self.context.notifications.is_some();
        // Paired phones that aren't connected aren't in the tray.
        let (reply, phones) = oneshot::channel();
        if self
            .context
            .requests
            .send(PhoneRequest::List { reply })
            .is_ok()
            && let Ok(Ok(paired)) = time::timeout(PHONE_LIST_TIMEOUT, phones).await
        {
            for (name, _) in paired {
                if !snapshot.phones.iter().any(|phone| phone.name == name) {
                    snapshot.phones.push(PhoneState {
                        fingerprint: String::new(),
                        name,
                        connected: false,
                        round_trip_ms: None,
                        accepts_audio: false,
                    });
                }
            }
            snapshot.phones.sort_by(|a, b| a.name.cmp(&b.name));
        }
        snapshot
    }

    async fn report(&self) -> String {
        let hello = hello(&self.context);
        let snapshot = self.snapshot().await;
        let mut report = format!(
            "edge-kvm {} (linux)\nconfig: {}\nstate: {}\nlog: {}\nstatus: {}\n",
            hello.backend_version,
            hello.config_path.display(),
            hello.state_dir.display(),
            hello.log_path.display(),
            snapshot.summary,
        );
        for diagnostic in &snapshot.diagnostics {
            report.push_str(&format!("{}: {}\n", diagnostic.key, diagnostic.value));
        }
        report.push_str(&format!(
            "last error: {}\nphones: {}\n",
            snapshot.last_error.as_deref().unwrap_or("none"),
            snapshot.phones.len()
        ));
        report
    }
}

fn snapshot_reply(snapshot: Snapshot) -> Outcome {
    Outcome::Ok(Reply::Snapshot(Box::new(snapshot)))
}

/// A receiver run with `--no-tray` has no published status.
pub(crate) fn headless_snapshot() -> Snapshot {
    Snapshot {
        summary: "Running without a tray; live status is unavailable".to_string(),
        phones_supported: true,
        capabilities: Capabilities {
            phones: true,
            listens: true,
            ..Capabilities::default()
        },
        ..Snapshot::default()
    }
}

/// Restarts with the saved settings, the same way the settings window does:
/// a helper process stops this one and starts a replacement (or lets the
/// service manager do so).
fn restart_receiver(context: &ControlContext) {
    let pid = std::process::id().to_string();
    let spawned = std::env::current_exe().and_then(|executable| {
        std::process::Command::new(executable)
            .arg("--restart-node")
            .arg(&pid)
            .arg("--config")
            .arg(&context.config_path)
            .spawn()
    });
    if let Err(error) = spawned {
        append_portable_log(
            &context.log_path,
            format!("couldn't start the restart helper: {error}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edge_control::{client::Client, framing::preamble};
    use tokio::{io::AsyncWriteExt, net::UnixStream};

    fn context(dir: &Path) -> (ControlContext, mpsc::UnboundedReceiver<PhoneRequest>) {
        let (requests, received) = mpsc::unbounded_channel();
        (
            ControlContext {
                requests,
                tray: None,
                notifications: None,
                config_path: dir.join("receiver.toml"),
                log_path: dir.join("receiver.log"),
            },
            received,
        )
    }

    /// Starts a server on one end of a socket pair and hands back a client
    /// on the other, after the leading NUL that routes it to this protocol.
    async fn connect(context: ControlContext) -> (Hello, Client, edge_control::client::Events) {
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let (mut reader, writer) = server.into_split();
            let mut first = [0; 1];
            reader.read_exact(&mut first).await.unwrap();
            serve(reader, writer, context).await.ok();
        });
        let (reader, writer) = client.into_split();
        Client::handshake(reader, writer).await.unwrap()
    }

    /// Answers phone list requests with nothing paired.
    fn answer_lists(mut requests: mpsc::UnboundedReceiver<PhoneRequest>) {
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if let PhoneRequest::List { reply } = request {
                    reply.send(Vec::new()).ok();
                }
            }
        });
    }

    #[tokio::test]
    async fn hello_identifies_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let (context, requests) = context(dir.path());
        answer_lists(requests);
        let (hello, client, _events) = connect(context).await;
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        assert_eq!(hello.pid, std::process::id());
        assert!(hello.config_path.ends_with("receiver.toml"));
        let Reply::Snapshot(snapshot) = client.request(Request::Subscribe).await.unwrap() else {
            panic!("expected a snapshot");
        };
        assert!(snapshot.revision > 0);
    }

    #[tokio::test]
    async fn other_protocol_versions_are_told_and_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _requests) = context(dir.path());
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let (mut reader, writer) = server.into_split();
            let mut first = [0; 1];
            reader.read_exact(&mut first).await.unwrap();
            serve(reader, writer, context).await.ok();
        });
        let (mut reader, mut writer) = client.into_split();
        writer
            .write_all(&preamble(PROTOCOL_VERSION + 1))
            .await
            .unwrap();
        let reply: ServerMessage = read_frame_async(&mut reader).await.unwrap();
        assert_eq!(
            reply,
            ServerMessage::Incompatible {
                server_protocol: PROTOCOL_VERSION
            }
        );
        assert!(matches!(
            read_frame_async::<ServerMessage>(&mut reader).await,
            Err(FrameError::Closed)
        ));
    }

    #[tokio::test]
    async fn malformed_requests_get_an_error_and_the_connection_survives() {
        let dir = tempfile::tempdir().unwrap();
        let (context, requests) = context(dir.path());
        answer_lists(requests);
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let (mut reader, writer) = server.into_split();
            let mut first = [0; 1];
            reader.read_exact(&mut first).await.unwrap();
            serve(reader, writer, context).await.ok();
        });
        let (mut reader, mut writer) = client.into_split();
        writer.write_all(&preamble(PROTOCOL_VERSION)).await.unwrap();
        let _hello: ServerMessage = read_frame_async(&mut reader).await.unwrap();
        let mut bad = 9u32.to_be_bytes().to_vec();
        bad.extend_from_slice(b"not json!");
        writer.write_all(&bad).await.unwrap();
        let reply: ServerMessage = read_frame_async(&mut reader).await.unwrap();
        assert!(matches!(
            reply,
            ServerMessage::Response {
                id: 0,
                outcome: Outcome::Error(ControlError {
                    kind: ErrorKind::BadRequest,
                    ..
                })
            }
        ));
        write_frame_async(
            &mut writer,
            &ClientMessage {
                id: 5,
                request: Request::Snapshot,
            },
        )
        .await
        .unwrap();
        let reply: ServerMessage = read_frame_async(&mut reader).await.unwrap();
        assert!(matches!(reply, ServerMessage::Response { id: 5, .. }));
    }

    #[tokio::test]
    async fn oversized_frames_close_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _requests) = context(dir.path());
        let (client, server) = UnixStream::pair().unwrap();
        let served = tokio::spawn(async move {
            let (mut reader, writer) = server.into_split();
            let mut first = [0; 1];
            reader.read_exact(&mut first).await.unwrap();
            serve(reader, writer, context).await
        });
        let (mut reader, mut writer) = client.into_split();
        writer.write_all(&preamble(PROTOCOL_VERSION)).await.unwrap();
        let _hello: ServerMessage = read_frame_async(&mut reader).await.unwrap();
        writer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(
            served.await.unwrap(),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn tray_actions_are_refused_without_a_tray() {
        let dir = tempfile::tempdir().unwrap();
        let (context, requests) = context(dir.path());
        answer_lists(requests);
        let (_, client, _events) = connect(context).await;
        let error = client
            .request(Request::SetInputForwarding { enabled: true })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            edge_control::client::RequestError::Control(ControlError {
                kind: ErrorKind::Unsupported,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn invalid_saves_leave_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (context, requests) = context(dir.path());
        answer_lists(requests);
        let config = AppConfig::receiver_default();
        config.save(&context.config_path).await.unwrap();
        let before = std::fs::read_to_string(&context.config_path).unwrap();
        let (_, client, _events) = connect(context.clone()).await;

        let base = SettingsDraft::from_config(&config);
        let mut draft = base.clone();
        draft.port = "0".into();
        let error = client
            .request(Request::SaveSettings {
                draft: draft.clone(),
                base: base.clone(),
            })
            .await
            .unwrap_err();
        let edge_control::client::RequestError::Control(error) = error else {
            panic!("expected a control error");
        };
        assert_eq!(error.kind, ErrorKind::Invalid);
        assert_eq!(error.fields[0].field, "port");
        assert_eq!(
            std::fs::read_to_string(&context.config_path).unwrap(),
            before
        );

        // A draft made from older settings is refused rather than applied.
        let mut stale = base.clone();
        stale.device_name = "Somewhere else".into();
        draft.port = "42421".into();
        let error = client
            .request(Request::SaveSettings { draft, base: stale })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            edge_control::client::RequestError::Control(ControlError {
                kind: ErrorKind::Conflict,
                ..
            })
        ));
        assert_eq!(
            std::fs::read_to_string(&context.config_path).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn validation_reports_fields_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (context, requests) = context(dir.path());
        answer_lists(requests);
        AppConfig::receiver_default()
            .save(&context.config_path)
            .await
            .unwrap();
        let (_, client, _events) = connect(context).await;
        let Reply::Settings(view) = client.request(Request::Settings).await.unwrap() else {
            panic!("expected settings");
        };
        let mut draft = view.draft;
        draft.device_name = String::new();
        let Reply::Validation { errors } = client
            .request(Request::ValidateSettings { draft })
            .await
            .unwrap()
        else {
            panic!("expected validation");
        };
        assert_eq!(errors[0].field, "device_name");
    }

    #[tokio::test]
    async fn phone_pairing_reports_the_phone_and_closing_cancels_it() {
        let dir = tempfile::tempdir().unwrap();
        let (context, mut requests) = context(dir.path());
        let (_, client, mut events) = connect(context.clone()).await;

        let request = tokio::spawn(async move {
            let offer = client.request(Request::StartPhonePairing).await.unwrap();
            (client, offer)
        });
        let Some(PhoneRequest::OpenPairing { reply, paired }) = requests.recv().await else {
            panic!("expected OpenPairing");
        };
        reply
            .send(PairingOffer {
                uri: "hyprmobile://pair?v=1".into(),
                expires_in: Duration::from_secs(300),
            })
            .ok();
        let (client, offer) = request.await.unwrap();
        assert_eq!(
            offer,
            Reply::PhonePairing(PhonePairingOffer {
                uri: "hyprmobile://pair?v=1".into(),
                expires_in_secs: 300
            })
        );
        paired.send("Pixel".into()).unwrap();
        assert_eq!(
            events.recv().await,
            Some(Event::PhonePairing(PhonePairingUpdate::Paired {
                name: "Pixel".into()
            }))
        );

        // A second pairing, abandoned by the UI going away.
        let request = tokio::spawn(async move {
            client.request(Request::StartPhonePairing).await.unwrap();
            client
        });
        let Some(PhoneRequest::OpenPairing { reply, paired }) = requests.recv().await else {
            panic!("expected OpenPairing");
        };
        reply
            .send(PairingOffer {
                uri: "hyprmobile://pair?v=1".into(),
                expires_in: Duration::from_secs(300),
            })
            .ok();
        drop(request.await.unwrap());
        assert!(matches!(
            requests.recv().await,
            Some(PhoneRequest::ClosePairing)
        ));
        assert!(paired.is_closed());
    }
}
