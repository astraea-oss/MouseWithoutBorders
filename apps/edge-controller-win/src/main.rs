#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::time::Duration;
use std::time::Instant;
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use edge_audio::SessionSecrets;
#[cfg(windows)]
use edge_clipboard::{
    ClipboardChangeTracker, ClipboardContentId, ClipboardError, ClipboardItem,
    IncomingImageTransfer, OutgoingImageTransfer,
};
use edge_common::PeerPosition;
use edge_common::{
    AppConfig, Role, TransportMode, default_state_dir, detect_primary_local_ip, init_tracing,
    portable_config_path,
};
use edge_crypto::{IdentityKey, NoiseSession, initiate_noise_session, pairing_code};
#[cfg(windows)]
use edge_geometry::Size;
use edge_protocol::Edge;
use edge_protocol::{
    AudioCodec, AudioControl, AudioStopReason, AudioStreamState, CLIPBOARD_IMAGE_EXTENSION,
    ClipboardCancelReason, ClipboardEvent, ControlEvent, Frame, Heartbeat, Hello,
    INITIAL_ROLE_EPOCH, INPUT_TOGGLE_EXTENSION, InputEvent, MouseButton, NodeCapability,
    PAIRING_CONFIRMATION_EXTENSION, PROTOCOL_VERSION, PairingEvent, RoleEvent, RoleState,
    RoleTransitionState, ScreenInfo, decode_frame, encode_frame,
};
use edge_runtime::{
    LivenessConfig, LivenessEvent, LivenessTracker, SecureFrameReader, SecureFrameSession,
    SecureFrameWriter,
};
use edge_ui::{PairingConfirmationInput, PairingUiState, SettingsUiInput};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    time,
};

type TcpFrameReader = SecureFrameReader<ReadHalf<TcpStream>>;
type ScheduledNoiseWriter = SecureFrameWriter<WriteHalf<TcpStream>>;

#[cfg(windows)]
const LIVE_INPUT_QUEUE_CAPACITY: usize = 32;
#[cfg(windows)]
const LIVE_INPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(windows)]
const FALLBACK_REMOTE_SIZE: Size = Size {
    width: 1920,
    height: 1080,
};
const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(10);
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CLIPBOARD_PASTE_BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const UPGRADE_PEER_STATUS: &str = "Upgrade the other computer";

#[derive(Debug, Parser)]
#[command(version, about = "Windows controller for edge-kvm")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, help = "Load config and connect without installing hooks")]
    dry_run: bool,
    #[arg(long, help = "Run the Windows tray shell after connecting")]
    tray: bool,
    #[arg(long, help = "Confirm and replace an unknown or changed laptop key")]
    pair: bool,
    #[arg(long, help = "Send one test input event over the encrypted session")]
    test_input: Option<TestInput>,
    #[arg(
        long,
        help = "Send one text clipboard offer over the encrypted session"
    )]
    test_clipboard_text: Option<String>,
    #[arg(long, help = "Play a local audio pipeline test tone")]
    test_audio: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TestInput {
    Pointer,
    Click,
    Wheel,
    Key,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let controller_log = default_state_dir().join("controller.log");
    install_controller_panic_log(controller_log.clone());
    append_portable_log(&controller_log, "controller process starting");

    let result = run_main(controller_log.clone()).await;
    match &result {
        Ok(()) => append_portable_log(&controller_log, "controller process exited cleanly"),
        Err(err) => append_portable_log(
            &controller_log,
            format!("controller process exited with error: {err:#}"),
        ),
    }
    #[cfg(windows)]
    edge_windows_input::force_release_to_local();
    result
}

async fn run_main(controller_log: PathBuf) -> Result<()> {
    let args = Args::parse();
    #[cfg(windows)]
    let run_tray = should_run_tray(&args);
    let config_path = args.config.unwrap_or_else(default_config_path);
    let mut config = load_or_create_config(&config_path).await?;

    if config.transport != TransportMode::Connect {
        anyhow::bail!(
            "controller requires transport = \"connect\" in {}",
            config_path.display()
        );
    }
    if !config.input.capture.output.trim().is_empty() {
        tracing::warn!(
            output = %config.input.capture.output,
            "input.capture.output is Linux-only; Windows keeps using the full virtual desktop"
        );
    }

    if args.test_audio {
        #[cfg(windows)]
        {
            tokio::task::spawn_blocking(edge_windows_audio::play_test_tone)
                .await
                .context("audio test task failed")??;
            return Ok(());
        }
        #[cfg(not(windows))]
        anyhow::bail!("the Windows audio test is only available on Windows");
    }

    let identity = IdentityKey::load_or_create(default_state_dir().join("identity.toml"))
        .await
        .context("failed to load controller identity")?;

    #[cfg(windows)]
    {
        if run_tray {
            let mut pairing_armed = args.pair;
            let (mut connection, pairing_consumed, connect_status) = connect_for_tray(
                &config,
                &identity,
                &config_path,
                &controller_log,
                pairing_armed,
            )
            .await;
            if connection.is_some() || pairing_consumed {
                pairing_armed = false;
            }
            let mut connection_enabled = connect_status != UPGRADE_PEER_STATUS;
            let mut input_forwarding_enabled = true;
            let mut next_connect_attempt = if connection.is_some() {
                Instant::now()
            } else {
                Instant::now() + Duration::from_secs(2)
            };
            let (tray_command_tx, mut tray_command_rx) = mpsc::unbounded_channel();
            let (win_tray_tx, win_tray_rx) = std_mpsc::channel();
            std::thread::spawn(move || {
                while let Ok(command) = win_tray_rx.recv() {
                    let _ = tray_command_tx.send(command);
                }
            });

            edge_windows_input::install_hooks().context("failed to install Windows hooks")?;
            let status = connection
                .as_ref()
                .map(|(connection, _)| connection.status())
                .unwrap_or(connect_status);
            tracing::info!(%status, "starting tray loop");
            append_portable_log(&controller_log, format!("starting tray loop: {status}"));
            let tray_log = controller_log.clone();
            std::thread::spawn(move || {
                if let Err(err) = edge_windows_input::run_tray(&status, win_tray_tx) {
                    tracing::warn!(%err, "Windows tray exited with error");
                    append_portable_log(
                        &tray_log,
                        format!("Windows tray exited with error: {err}"),
                    );
                }
            });
            update_windows_tray_audio(
                config.audio.enabled,
                if config.audio.enabled {
                    "Audio: Waiting for connection"
                } else {
                    "Audio: Off"
                },
                &controller_log,
            );
            update_windows_tray_input_forwarding(true, &controller_log);

            loop {
                match handle_pending_windows_tray_commands(
                    &mut tray_command_rx,
                    &config_path,
                    &config,
                    &controller_log,
                    input_forwarding_enabled,
                )? {
                    TrayCommandOutcome::Quit => return Ok(()),
                    TrayCommandOutcome::Disconnect => {
                        connection_enabled = false;
                        connection = None;
                        edge_windows_input::force_release_to_local();
                        update_windows_tray_status("Disconnected by user", &controller_log);
                        update_windows_tray_audio(
                            config.audio.enabled,
                            if config.audio.enabled {
                                "Audio: Paused"
                            } else {
                                "Audio: Off"
                            },
                            &controller_log,
                        );
                    }
                    TrayCommandOutcome::Reconnect => {
                        connection_enabled = true;
                        next_connect_attempt = Instant::now();
                        update_windows_tray_status("Connecting", &controller_log);
                    }
                    TrayCommandOutcome::Pair => {
                        pairing_armed = true;
                        connection_enabled = true;
                        connection = None;
                        next_connect_attempt = Instant::now();
                        update_windows_tray_status(
                            "Pairing: enable pairing on the laptop",
                            &controller_log,
                        );
                    }
                    TrayCommandOutcome::InputForwardingChanged(enabled) => {
                        input_forwarding_enabled = enabled;
                        update_windows_tray_input_forwarding(enabled, &controller_log);
                    }
                    TrayCommandOutcome::AudioChanged(enabled) => {
                        config.audio.enabled = enabled;
                        update_windows_tray_audio(
                            enabled,
                            if enabled && connection_enabled {
                                "Audio: Starting"
                            } else if enabled {
                                "Audio: Paused"
                            } else {
                                "Audio: Off"
                            },
                            &controller_log,
                        );
                    }
                    TrayCommandOutcome::Continue => {}
                }

                if let Some((active_connection, screen_info)) = connection.take() {
                    update_windows_tray_status(&active_connection.status(), &controller_log);
                    match run_connected(
                        active_connection,
                        &config,
                        screen_info.screen_info,
                        screen_info
                            .node_capabilities
                            .contains(&NodeCapability::AudioCaptureV1),
                        screen_info
                            .extensions
                            .iter()
                            .any(|extension| extension == INPUT_TOGGLE_EXTENSION),
                        screen_info
                            .extensions
                            .iter()
                            .any(|extension| extension == CLIPBOARD_IMAGE_EXTENSION),
                        &mut input_forwarding_enabled,
                        &controller_log,
                        &config_path,
                        Some(&mut tray_command_rx),
                    )
                    .await
                    {
                        Ok(ConnectedSessionExit::Quit) => return Ok(()),
                        Ok(ConnectedSessionExit::Disconnect) => {
                            connection_enabled = false;
                            update_windows_tray_status("Disconnected by user", &controller_log);
                            update_windows_tray_audio(
                                config.audio.enabled,
                                if config.audio.enabled {
                                    "Audio: Paused"
                                } else {
                                    "Audio: Off"
                                },
                                &controller_log,
                            );
                        }
                        Ok(ConnectedSessionExit::Pair) => {
                            pairing_armed = true;
                            connection_enabled = true;
                            next_connect_attempt = Instant::now();
                            update_windows_tray_status(
                                "Pairing: enable pairing on the laptop",
                                &controller_log,
                            );
                        }
                        Err(err) => {
                            tracing::warn!(%err, "connected session ended; reconnecting");
                            append_portable_log(
                                &controller_log,
                                format!("connected session ended; reconnecting: {err:#}"),
                            );
                            update_windows_tray_status("Disconnected", &controller_log);
                            update_windows_tray_audio(
                                config.audio.enabled,
                                if config.audio.enabled {
                                    "Audio: Waiting for connection"
                                } else {
                                    "Audio: Off"
                                },
                                &controller_log,
                            );
                            next_connect_attempt = Instant::now() + Duration::from_secs(2);
                        }
                    }
                }

                time::sleep(Duration::from_millis(250)).await;
                if !connection_enabled {
                    connection = None;
                    continue;
                }
                if Instant::now() < next_connect_attempt {
                    continue;
                }
                config = load_or_create_config(&config_path).await?;
                let (next_connection, pairing_consumed, connect_status) = connect_for_tray(
                    &config,
                    &identity,
                    &config_path,
                    &controller_log,
                    pairing_armed,
                )
                .await;
                connection = next_connection;
                if connection.is_some() || pairing_consumed {
                    pairing_armed = false;
                }
                if connection.is_none() {
                    if connect_status == UPGRADE_PEER_STATUS {
                        connection_enabled = false;
                    } else {
                        next_connect_attempt = Instant::now() + Duration::from_secs(2);
                    }
                }
                let status = connection
                    .as_ref()
                    .map(|(connection, _)| connection.status())
                    .unwrap_or(connect_status);
                update_windows_tray_status(&status, &controller_log);
            }
        }
    }

    let mut pairing_consumed = false;
    let (mut connection, initial_receiver) = connect_and_initialize(
        &config,
        &identity,
        &config_path,
        &controller_log,
        args.pair,
        &mut pairing_consumed,
    )
    .await?;

    if let Some(test) = args.test_input {
        send_test_input(&mut connection.session, test).await?;
        drain_for(Duration::from_millis(500), &mut connection.session).await;
        return Ok(());
    }

    if let Some(text) = args.test_clipboard_text {
        write_secure_frame(
            &mut connection.session,
            &Frame::Clipboard(ClipboardEvent::TextOffer { sequence: 1, text }),
        )
        .await?;
        drain_for(Duration::from_millis(500), &mut connection.session).await;
        return Ok(());
    }

    if args.dry_run {
        tracing::info!(status = %connection.status(), "dry-run connection succeeded");
        return Ok(());
    }

    #[cfg(windows)]
    {
        let mut input_forwarding_enabled = true;
        run_connected(
            connection,
            &config,
            initial_receiver.screen_info,
            initial_receiver
                .node_capabilities
                .contains(&NodeCapability::AudioCaptureV1),
            initial_receiver
                .extensions
                .iter()
                .any(|extension| extension == INPUT_TOGGLE_EXTENSION),
            initial_receiver
                .extensions
                .iter()
                .any(|extension| extension == CLIPBOARD_IMAGE_EXTENSION),
            &mut input_forwarding_enabled,
            &controller_log,
            &config_path,
            None,
        )
        .await?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = initial_receiver;
        anyhow::bail!("live controller mode is only available on Windows; use --dry-run")
    }
}

#[cfg(windows)]
fn should_run_tray(args: &Args) -> bool {
    args.tray || (!args.dry_run && args.test_input.is_none() && args.test_clipboard_text.is_none())
}

#[cfg(windows)]
async fn connect_for_tray(
    config: &AppConfig,
    identity: &IdentityKey,
    config_path: &Path,
    log_path: &Path,
    pairing_armed: bool,
) -> (
    Option<(ControllerConnection, InitialReceiverState)>,
    bool,
    String,
) {
    let mut pairing_consumed = false;
    match connect_and_initialize(
        config,
        identity,
        config_path,
        log_path,
        pairing_armed,
        &mut pairing_consumed,
    )
    .await
    {
        Ok(connection) => {
            let status = connection.0.status();
            (Some(connection), pairing_consumed, status)
        }
        Err(err) => {
            tracing::warn!(%err, "tray connection attempt failed");
            append_portable_log(log_path, format!("tray connection attempt failed: {err:#}"));
            let message = format!("{err:#}");
            let status = if message.contains("fingerprint mismatch") {
                "Identity changed — pair again".to_string()
            } else if message.contains("pairing") || message.contains("not paired") {
                "Pairing required on both computers".to_string()
            } else if message.contains(UPGRADE_PEER_STATUS) {
                UPGRADE_PEER_STATUS.to_string()
            } else {
                "Disconnected".to_string()
            };
            (None, pairing_consumed, status)
        }
    }
}

fn append_portable_log(path: &Path, message: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{:?} {}", SystemTime::now(), message.as_ref());
    }
}

#[cfg(windows)]
fn update_windows_tray_status(status: &str, log_path: &Path) {
    if let Err(err) = edge_windows_input::update_tray_status(status) {
        tracing::warn!(%err, status, "failed to update Windows tray status");
        append_portable_log(
            log_path,
            format!("failed to update Windows tray status to {status}: {err}"),
        );
    }
}

#[cfg(windows)]
fn update_windows_tray_audio(enabled: bool, status: &str, log_path: &Path) {
    if let Err(err) = edge_windows_input::update_tray_audio(enabled, status) {
        tracing::warn!(%err, enabled, status, "failed to update Windows tray audio state");
        append_portable_log(
            log_path,
            format!("failed to update Windows tray audio state to {status}: {err}"),
        );
    }
}

#[cfg(windows)]
fn update_windows_tray_input_forwarding(enabled: bool, log_path: &Path) {
    if let Err(err) = edge_windows_input::update_tray_input_forwarding(enabled) {
        tracing::warn!(%err, enabled, "failed to update Windows tray input forwarding state");
        append_portable_log(
            log_path,
            format!("failed to update Windows tray input forwarding state to {enabled}: {err}"),
        );
    }
}

#[cfg(windows)]
enum TrayCommandOutcome {
    Continue,
    Quit,
    Disconnect,
    Reconnect,
    Pair,
    InputForwardingChanged(bool),
    AudioChanged(bool),
}

#[cfg(windows)]
fn handle_pending_windows_tray_commands(
    commands: &mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>,
    config_path: &Path,
    config: &AppConfig,
    log_path: &Path,
    input_forwarding_enabled: bool,
) -> Result<TrayCommandOutcome> {
    while let Ok(command) = commands.try_recv() {
        match command {
            edge_windows_input::WindowsTrayCommand::OpenSettings => {
                let config = AppConfig::load_blocking(config_path).unwrap_or_else(|err| {
                    tracing::warn!(%err, "failed to reload config for settings UI");
                    config.clone()
                });
                append_portable_log(log_path, "opening settings window");
                edge_ui::spawn_settings_window(SettingsUiInput {
                    role: Role::Controller,
                    config_path: config_path.to_path_buf(),
                    local_ip: detect_primary_local_ip(),
                    pairing: controller_pairing_state(&config),
                    config,
                });
            }
            edge_windows_input::WindowsTrayCommand::Pair => {
                append_portable_log(log_path, "pairing requested from tray");
                return Ok(TrayCommandOutcome::Pair);
            }
            edge_windows_input::WindowsTrayCommand::ReleaseControl => {
                edge_windows_input::release_to_local(edge_protocol::ReleaseReason::UserRequest);
            }
            edge_windows_input::WindowsTrayCommand::Disconnect => {
                append_portable_log(log_path, "disconnect requested from tray");
                return Ok(TrayCommandOutcome::Disconnect);
            }
            edge_windows_input::WindowsTrayCommand::Reconnect => {
                append_portable_log(log_path, "reconnect requested from tray");
                return Ok(TrayCommandOutcome::Reconnect);
            }
            edge_windows_input::WindowsTrayCommand::ToggleInputForwarding => {
                let enabled = !input_forwarding_enabled;
                append_portable_log(log_path, format!("input forwarding toggled to {enabled}"));
                return Ok(TrayCommandOutcome::InputForwardingChanged(enabled));
            }
            edge_windows_input::WindowsTrayCommand::ToggleAudio => {
                let mut updated = AppConfig::load_blocking(config_path).unwrap_or_else(|err| {
                    tracing::warn!(%err, "failed to reload config for audio toggle");
                    config.clone()
                });
                updated.audio.enabled = !updated.audio.enabled;
                updated.save_blocking(config_path)?;
                append_portable_log(
                    log_path,
                    format!("audio streaming toggled to {}", updated.audio.enabled),
                );
                return Ok(TrayCommandOutcome::AudioChanged(updated.audio.enabled));
            }
            edge_windows_input::WindowsTrayCommand::Quit => {
                append_portable_log(log_path, "quit requested from tray");
                return Ok(TrayCommandOutcome::Quit);
            }
        }
    }

    Ok(TrayCommandOutcome::Continue)
}

#[cfg(windows)]
fn controller_pairing_state(config: &AppConfig) -> PairingUiState {
    if !config.peer.pinned_fingerprint.trim().is_empty() {
        return PairingUiState::Paired {
            peer_name: config.peer.name.clone(),
            peer_fingerprint: config.peer.pinned_fingerprint.clone(),
        };
    }

    PairingUiState::Idle
}

fn install_controller_panic_log(log_path: PathBuf) {
    std::panic::set_hook(Box::new(move |panic_info| {
        append_portable_log(&log_path, format!("controller panic: {panic_info}"));
        #[cfg(windows)]
        edge_windows_input::force_release_to_local();
    }));
}

struct ControllerConnection {
    session: NoiseSession<TcpStream>,
    addr: String,
    peer_addr: std::net::SocketAddr,
    peer_fingerprint: String,
    peer_trusted: bool,
    pairing_armed: bool,
}

impl ControllerConnection {
    fn status(&self) -> String {
        format!("Connected to {} ({})", self.addr, self.peer_fingerprint)
    }
}

async fn connect_and_initialize(
    config: &AppConfig,
    identity: &IdentityKey,
    config_path: &Path,
    log_path: &Path,
    pairing_armed: bool,
    pairing_consumed: &mut bool,
) -> Result<(ControllerConnection, InitialReceiverState)> {
    let mut connection = connect_session(config, identity, pairing_armed).await?;
    let hello = time::timeout(Duration::from_secs(15), async {
        loop {
            match read_secure_frame(&mut connection.session).await? {
                Frame::Hello(hello) => break Ok(hello),
                Frame::Error(error) => {
                    anyhow::bail!("receiver error: {}: {}", error.code, error.message)
                }
                frame => tracing::debug!(?frame, "waiting for receiver hello"),
            }
        }
    })
    .await
    .context("timed out waiting for receiver hello")??;
    validate_receiver_hello(&hello, &connection.peer_fingerprint)?;

    let mut initial = InitialReceiverState {
        screen_info: None,
        node_capabilities: hello.node_capabilities.clone(),
        extensions: hello.extensions.clone(),
    };
    let supports_confirmation = hello
        .extensions
        .iter()
        .any(|extension| extension == PAIRING_CONFIRMATION_EXTENSION);

    if supports_confirmation {
        write_secure_frame(
            &mut connection.session,
            &Frame::Pairing(PairingEvent::Status {
                trusted: connection.peer_trusted,
                armed: connection.pairing_armed,
            }),
        )
        .await?;
        let (peer_trusted, peer_armed) = read_pairing_status(&mut connection.session).await?;
        if !connection.peer_trusted || !peer_trusted {
            if !connection.pairing_armed || !peer_armed {
                write_secure_frame(
                    &mut connection.session,
                    &Frame::Pairing(PairingEvent::Decision { accepted: false }),
                )
                .await
                .ok();
                anyhow::bail!(
                    "pairing needs approval on both computers; choose the pairing action in both tray menus"
                );
            }

            #[cfg(windows)]
            update_windows_tray_status("Pairing: compare the code on both computers", log_path);
            let peer = &config.peer;
            let previous_peer_fingerprint = (!peer.pinned_fingerprint.is_empty()
                && peer.pinned_fingerprint != connection.peer_fingerprint)
                .then(|| peer.pinned_fingerprint.clone());
            let confirmation = PairingConfirmationInput {
                peer_name: hello.device_name.clone(),
                peer_addr: Some(connection.peer_addr.to_string()),
                local_fingerprint: identity.fingerprint(),
                peer_fingerprint: connection.peer_fingerprint.clone(),
                verification_code: pairing_code(
                    &identity.fingerprint(),
                    &connection.peer_fingerprint,
                ),
                previous_peer_fingerprint,
            };
            *pairing_consumed = true;
            let accepted = tokio::task::spawn_blocking(move || {
                edge_ui::run_pairing_confirmation(confirmation)
            })
            .await
            .context("controller pairing confirmation task failed")??;
            write_secure_frame(
                &mut connection.session,
                &Frame::Pairing(PairingEvent::Decision { accepted }),
            )
            .await?;
            if !accepted {
                anyhow::bail!("pairing was cancelled on this computer");
            }
            if !read_pairing_decision(&mut connection.session).await? {
                anyhow::bail!("pairing was declined on the laptop");
            }

            let mut updated = AppConfig::load(config_path).await.unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to reload controller config before saving pin");
                config.clone()
            });
            updated.peer.pinned_fingerprint = connection.peer_fingerprint.clone();
            updated.save(config_path).await?;
            connection.peer_trusted = true;
            append_portable_log(
                log_path,
                format!(
                    "paired laptop {} ({}) after confirmation on both computers",
                    hello.device_name, connection.peer_fingerprint
                ),
            );
        }
    } else if !connection.peer_trusted {
        anyhow::bail!(
            "the laptop does not support two-sided pairing confirmation; update it before replacing its key"
        );
    }

    write_secure_frame(
        &mut connection.session,
        &Frame::Role(RoleEvent::SessionState(RoleState {
            controller_fingerprint: Some(identity.fingerprint()),
            role_epoch: INITIAL_ROLE_EPOCH,
            transition: RoleTransitionState::Stable,
            listener_position: peer_position_to_edge(config.layout.listener_position),
            paused: false,
            failure_detail: None,
        })),
    )
    .await?;
    #[cfg(windows)]
    write_secure_frame(
        &mut connection.session,
        &Frame::ScreenInfo(edge_windows_input::screen_info()),
    )
    .await?;

    read_initial_frames(&mut connection.session, &mut initial).await?;
    Ok((connection, initial))
}

fn validate_receiver_hello(hello: &Hello, noise_fingerprint: &str) -> Result<()> {
    if hello.protocol_version != PROTOCOL_VERSION {
        anyhow::bail!(
            "Upgrade the other computer: receiver protocol version {} is incompatible with {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        );
    }
    if hello.public_key_fingerprint != noise_fingerprint {
        anyhow::bail!("receiver hello fingerprint does not match its encrypted identity");
    }
    Ok(())
}

async fn read_pairing_status(session: &mut NoiseSession<TcpStream>) -> Result<(bool, bool)> {
    match time::timeout(Duration::from_secs(15), read_secure_frame(session)).await {
        Ok(Ok(Frame::Pairing(PairingEvent::Status { trusted, armed }))) => Ok((trusted, armed)),
        Ok(Ok(Frame::Error(error))) => {
            anyhow::bail!("receiver error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected receiver pairing status, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read receiver pairing status"),
        Err(_) => anyhow::bail!("timed out waiting for receiver pairing status"),
    }
}

async fn read_pairing_decision(session: &mut NoiseSession<TcpStream>) -> Result<bool> {
    match time::timeout(Duration::from_secs(120), read_secure_frame(session)).await {
        Ok(Ok(Frame::Pairing(PairingEvent::Decision { accepted }))) => Ok(accepted),
        Ok(Ok(Frame::Error(error))) => {
            anyhow::bail!("receiver error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected receiver pairing decision, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read receiver pairing decision"),
        Err(_) => anyhow::bail!("timed out waiting for pairing confirmation on the laptop"),
    }
}

async fn connect_session(
    config: &AppConfig,
    identity: &IdentityKey,
    pairing_armed: bool,
) -> Result<ControllerConnection> {
    let peer = &config.peer;
    let addr = format!("{}:{}", peer.host, peer.port);
    let stream = time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .with_context(|| format!("connection to {addr} timed out"))?
        .with_context(|| format!("failed to connect to {addr}"))?;
    let peer_addr = stream
        .peer_addr()
        .context("failed to query receiver address")?;
    let expected_fingerprint = (!pairing_armed).then_some(peer.pinned_fingerprint.as_str());
    let (mut session, peer_fingerprint) = time::timeout(
        Duration::from_secs(15),
        initiate_noise_session(stream, identity, expected_fingerprint),
    )
    .await
    .context("encrypted handshake timed out")?
            .with_context(|| {
                format!(
                    "failed encrypted handshake with {addr}; if this laptop was reset, choose 'Pair or replace laptop...' from the tray on both computers"
                )
            })?;
    let peer_trusted =
        !peer.pinned_fingerprint.is_empty() && peer.pinned_fingerprint == peer_fingerprint;
    if !peer_trusted && !pairing_armed {
        anyhow::bail!(
            "the laptop at {addr} is not paired; choose 'Pair or replace laptop...' from both tray menus"
        );
    }

    write_secure_frame(
        &mut session,
        &Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: config.device_name.clone(),
            role: config.preferred_role,
            public_key_fingerprint: identity.fingerprint(),
            capabilities: Vec::new(),
            extensions: vec![
                CLIPBOARD_IMAGE_EXTENSION.to_string(),
                INPUT_TOGGLE_EXTENSION.to_string(),
                PAIRING_CONFIRMATION_EXTENSION.to_string(),
            ],
            node_capabilities: vec![
                NodeCapability::InputCaptureV1,
                NodeCapability::ScreenInfoBothSidesV1,
                NodeCapability::AudioPlaybackV1,
            ],
        }),
    )
    .await?;

    tracing::info!(%addr, %peer_fingerprint, "sent encrypted controller hello");
    Ok(ControllerConnection {
        session,
        addr,
        peer_addr,
        peer_fingerprint,
        peer_trusted,
        pairing_armed,
    })
}

#[derive(Debug, Default)]
struct InitialReceiverState {
    screen_info: Option<ScreenInfo>,
    node_capabilities: Vec<NodeCapability>,
    extensions: Vec<String>,
}

async fn read_initial_frames(
    session: &mut NoiseSession<TcpStream>,
    initial: &mut InitialReceiverState,
) -> Result<()> {
    loop {
        match read_secure_frame(session).await {
            Ok(Frame::Hello(hello)) => {
                tracing::info!(
                    device = %hello.device_name,
                    fingerprint = %hello.public_key_fingerprint,
                    "receiver hello"
                );
                initial.node_capabilities = hello.node_capabilities;
                initial.extensions = hello.extensions;
            }
            Ok(Frame::ScreenInfo(info)) => {
                tracing::info!(
                    primary = %info.primary_output,
                    outputs = info.outputs.len(),
                    "receiver screen info"
                );
                initial.screen_info = Some(info);
                return Ok(());
            }
            Ok(Frame::Heartbeat(_)) => return Ok(()),
            Ok(Frame::Error(err)) => {
                anyhow::bail!("receiver error: {}: {}", err.code, err.message)
            }
            Ok(frame) => tracing::debug!(?frame, "initial receiver frame"),
            Err(err) => return Err(err).context("failed to read receiver frame"),
        }
    }
}

async fn send_test_input(session: &mut NoiseSession<TcpStream>, test: TestInput) -> Result<()> {
    match test {
        TestInput::Pointer => {
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::PointerMotion { dx: 80.0, dy: 0.0 },
                ),
            )
            .await?;
        }
        TestInput::Click => {
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::PointerButton {
                        button: MouseButton::Left,
                        down: true,
                    },
                ),
            )
            .await?;
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::PointerButton {
                        button: MouseButton::Left,
                        down: false,
                    },
                ),
            )
            .await?;
        }
        TestInput::Wheel => {
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::PointerWheel { x: 0.0, y: -1.0 },
                ),
            )
            .await?;
        }
        TestInput::Key => {
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::Key {
                        evdev_code: 30,
                        down: true,
                    },
                ),
            )
            .await?;
            write_secure_frame(
                session,
                &Frame::input(
                    INITIAL_ROLE_EPOCH,
                    InputEvent::Key {
                        evdev_code: 30,
                        down: false,
                    },
                ),
            )
            .await?;
        }
    }

    write_secure_frame(
        session,
        &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
    )
    .await?;
    tracing::info!(?test, "sent test input");
    Ok(())
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectedSessionExit {
    Quit,
    Disconnect,
    Pair,
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn run_connected(
    connection: ControllerConnection,
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
    peer_supports_audio: bool,
    peer_supports_input_toggle: bool,
    peer_supports_images: bool,
    input_forwarding_enabled: &mut bool,
    log_path: &Path,
    config_path: &Path,
    tray_commands: Option<&mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>>,
) -> Result<ConnectedSessionExit> {
    let result = run_connected_inner(
        connection,
        config,
        screen_info,
        peer_supports_audio,
        peer_supports_input_toggle,
        peer_supports_images,
        input_forwarding_enabled,
        log_path,
        config_path,
        tray_commands,
    )
    .await;
    edge_windows_input::force_release_to_local();
    result
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn run_connected_inner(
    connection: ControllerConnection,
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
    peer_supports_audio: bool,
    peer_supports_input_toggle: bool,
    peer_supports_images: bool,
    input_forwarding_enabled: &mut bool,
    log_path: &Path,
    config_path: &Path,
    mut tray_commands: Option<&mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>>,
) -> Result<ConnectedSessionExit> {
    tracing::info!(status = %connection.status(), "connected; press Ctrl+C to quit");
    append_portable_log(
        log_path,
        format!("connected session started: {}", connection.status()),
    );
    let mut input_rx = start_live_input(config, screen_info)?;
    edge_windows_input::set_forwarding_enabled(*input_forwarding_enabled);
    update_windows_tray_input_forwarding(*input_forwarding_enabled, log_path);
    let mut runtime_config = config.clone();
    let mut live_clipboard = LiveClipboardState::new(&runtime_config, peer_supports_images).await?;
    let mut clipboard_poll = time::interval(CLIPBOARD_POLL_INTERVAL);
    clipboard_poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut clipboard_send = time::interval(Duration::from_millis(2));
    clipboard_send.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut stats = ControllerInputStats::default();
    let mut status_log = time::interval(STATUS_LOG_INTERVAL);
    let mut config_refresh = time::interval(Duration::from_millis(500));
    let mut audio_watch = time::interval(Duration::from_millis(500));
    let (clipboard_tx, mut clipboard_rx) = mpsc::unbounded_channel();
    let (reader, mut writer) = SecureFrameSession::new(connection.session).split();
    let mut receiver_rx = spawn_receiver_reader(reader);
    let mut tray_command_poll = time::interval(Duration::from_millis(200));
    let liveness_config = LivenessConfig::default();
    let mut heartbeat = time::interval(liveness_config.heartbeat_interval(true));
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut heartbeat_sequence = 0_u64;
    let mut connection_watchdog = time::interval(Duration::from_millis(250));
    connection_watchdog.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut receiver_liveness = LivenessTracker::new(liveness_config, tokio::time::Instant::now());
    let peer_ip = connection.peer_addr.ip();
    let mut _audio_receiver: Option<edge_windows_audio::WindowsAudioReceiver> = None;
    let mut audio_restart_attempted = false;
    let mut audio_enabled = runtime_config.audio.enabled;
    if peer_supports_audio {
        append_portable_log(
            log_path,
            format!("requesting Linux audio enabled={audio_enabled}"),
        );
        update_windows_tray_audio(
            audio_enabled,
            if audio_enabled {
                "Audio: Starting"
            } else {
                "Audio: Off"
            },
            log_path,
        );
        write_secure_frame_writer(
            &mut writer,
            &Frame::Audio(AudioControl::SetEnabled {
                enabled: audio_enabled,
            }),
        )
        .await?;
    } else {
        append_portable_log(log_path, "receiver does not advertise AudioV1");
        update_windows_tray_audio(audio_enabled, "Audio: Unsupported by receiver", log_path);
    }
    if peer_supports_input_toggle {
        write_secure_frame_writer(
            &mut writer,
            &Frame::control(
                INITIAL_ROLE_EPOCH,
                ControlEvent::SetInputForwarding {
                    enabled: *input_forwarding_enabled,
                },
            ),
        )
        .await?;
    }

    loop {
        if writer.bulk_is_due()
            && let Some((frame, completed)) = live_clipboard.next_image_frame()
        {
            write_secure_frame_writer(&mut writer, &frame).await?;
            if completed {
                stats.clipboard = stats.clipboard.saturating_add(1);
                tracing::info!("completed Windows clipboard image transfer to receiver");
                for held in live_clipboard.take_held_input() {
                    write_secure_frame_writer(&mut writer, &held).await?;
                    stats.record_frame(&held);
                    live_clipboard.after_input_sent(&held, &runtime_config, &clipboard_tx);
                }
            }
            continue;
        }

        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                write_secure_frame_writer(
                    &mut writer,
                    &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
                )
                .await
                .ok();
                if peer_supports_audio {
                    write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Stop { reason: AudioStopReason::Shutdown })).await.ok();
                }
                tracing::info!("shutdown requested");
                append_portable_log(log_path, "shutdown requested");
                return Ok(ConnectedSessionExit::Quit);
            },
            _ = status_log.tick() => {
                stats.log(log_path, "controller");
                if let Some(receiver) = &_audio_receiver {
                    let audio = receiver.stats();
                    append_portable_log(
                        log_path,
                        format!(
                            "Windows audio status authenticated={} rejected={} late={} concealed={} output_underruns={} dropped_output_frames={} queued_output_ms={}",
                            audio.authenticated_packets,
                            audio.rejected_packets,
                            audio.late_packets,
                            audio.concealed_packets,
                            audio.output_underruns,
                            audio.dropped_output_frames,
                            audio.queued_output_ms,
                        ),
                    );
                }
            },
            _ = audio_watch.tick() => {
                if _audio_receiver.as_ref().is_some_and(|receiver| receiver.is_finished()) {
                    let failure = _audio_receiver
                        .take()
                        .expect("finished Windows audio receiver disappeared")
                        .failure_reason()
                        .await;
                    if audio_enabled && peer_supports_audio && !audio_restart_attempted {
                        audio_restart_attempted = true;
                        update_windows_tray_audio(true, "Audio: Restarting", log_path);
                        append_portable_log(log_path, format!("Windows audio media stopped ({failure}); requesting one restart"));
                        write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::SetEnabled { enabled: true })).await?;
                    } else {
                        update_windows_tray_audio(audio_enabled, &format!("Audio error: {failure}"), log_path);
                        append_portable_log(log_path, format!("Windows audio transport stopped: {failure}"));
                        write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Stop { reason: AudioStopReason::TransportFailure })).await.ok();
                    }
                }
            },
            _ = config_refresh.tick() => {
                match AppConfig::load(config_path).await {
                    Ok(updated) => {
                        if updated.audio.enabled != audio_enabled {
                            audio_enabled = updated.audio.enabled;
                            audio_restart_attempted = false;
                            if !audio_enabled {
                                _audio_receiver = None;
                            }
                            update_windows_tray_audio(
                                audio_enabled,
                                if audio_enabled && peer_supports_audio { "Audio: Starting" } else if audio_enabled { "Audio: Unsupported by receiver" } else { "Audio: Off" },
                                log_path,
                            );
                            append_portable_log(log_path, format!("applied saved audio setting enabled={audio_enabled}"));
                            if peer_supports_audio {
                                write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::SetEnabled { enabled: audio_enabled })).await?;
                            }
                        }
                        runtime_config = updated;
                    }
                    Err(error) => tracing::warn!(%error, "failed to reload controller config"),
                }
            },
            _ = connection_watchdog.tick() => {
                if let Some(transfer_id) = live_clipboard.incoming.expire_transfer_id() {
                    tracing::warn!(transfer_id, "expired incomplete receiver clipboard image");
                    write_secure_frame_writer(
                        &mut writer,
                        &Frame::Clipboard(ClipboardEvent::ImageCancel {
                            transfer_id,
                            reason: ClipboardCancelReason::TimedOut,
                        }),
                    )
                    .await?;
                }
                match receiver_liveness.poll(tokio::time::Instant::now()) {
                    Some(LivenessEvent::SoftInputTimeout) => {
                        edge_windows_input::force_release_to_local();
                        tracing::warn!("receiver was silent for one second; released local capture while keeping the session available");
                        append_portable_log(log_path, "receiver liveness soft timeout; released local capture");
                    }
                    Some(LivenessEvent::HardSessionTimeout) => {
                        anyhow::bail!(
                            "receiver stopped responding for {:?}; reconnecting",
                            receiver_liveness.elapsed(tokio::time::Instant::now())
                        );
                    }
                    None => {}
                }
            },
            _ = heartbeat.tick() => {
                heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                write_secure_frame_writer(
                    &mut writer,
                    &Frame::Heartbeat(Heartbeat { sequence: heartbeat_sequence }),
                )
                .await?;
            },
            _ = clipboard_poll.tick() => {
                match live_clipboard.local_change_offer(&runtime_config).await {
                    Ok(frames) => for frame in frames {
                        write_secure_frame_writer(&mut writer, &frame).await?;
                        stats.record_frame(&frame);
                        tracing::info!("sent changed Windows clipboard to receiver");
                    },
                    Err(error) => {
                        tracing::debug!(%error, "skipped Windows clipboard poll");
                    }
                }
            },
            _ = tray_command_poll.tick(), if tray_commands.is_some() => {
                if let Some(commands) = tray_commands.as_deref_mut() {
                    match handle_pending_windows_tray_commands(
                        commands,
                        config_path,
                        config,
                        log_path,
                        *input_forwarding_enabled,
                    )? {
                        TrayCommandOutcome::Quit => {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            if peer_supports_audio {
                                write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Stop { reason: AudioStopReason::Shutdown })).await.ok();
                            }
                            return Ok(ConnectedSessionExit::Quit);
                        }
                        TrayCommandOutcome::Disconnect => {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            if peer_supports_audio {
                                write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Stop { reason: AudioStopReason::UserRequest })).await.ok();
                            }
                            _audio_receiver = None;
                            edge_windows_input::force_release_to_local();
                            return Ok(ConnectedSessionExit::Disconnect);
                        }
                        TrayCommandOutcome::Reconnect => {}
                        TrayCommandOutcome::Pair => {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            edge_windows_input::force_release_to_local();
                            return Ok(ConnectedSessionExit::Pair);
                        }
                        TrayCommandOutcome::InputForwardingChanged(enabled) => {
                            if !enabled {
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::input(INITIAL_ROLE_EPOCH, InputEvent::AllKeysUp),
                                )
                                .await?;
                            }
                            *input_forwarding_enabled = enabled;
                            edge_windows_input::set_forwarding_enabled(enabled);
                            update_windows_tray_input_forwarding(enabled, log_path);
                            if peer_supports_input_toggle {
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::control(
                                        INITIAL_ROLE_EPOCH,
                                        ControlEvent::SetInputForwarding { enabled },
                                    ),
                                )
                                .await?;
                            }
                        }
                        TrayCommandOutcome::AudioChanged(enabled) => {
                            audio_enabled = enabled;
                            audio_restart_attempted = false;
                            if !enabled { _audio_receiver = None; }
                            runtime_config.audio.enabled = enabled;
                            update_windows_tray_audio(enabled, if enabled { "Audio: Starting" } else { "Audio: Off" }, log_path);
                            if peer_supports_audio {
                                write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::SetEnabled { enabled })).await?;
                            }
                        }
                        TrayCommandOutcome::Continue => {}
                    }
                }
            },
            event = recv_live_input(&mut input_rx) => {
                if *input_forwarding_enabled && let Some(frame) = event {
                    for prepared in live_clipboard.prepare_input(frame, &runtime_config).await? {
                        write_secure_frame_writer(&mut writer, &prepared).await?;
                        stats.record_frame(&prepared);
                        live_clipboard.after_input_sent(&prepared, &runtime_config, &clipboard_tx);
                    }
                }
            },
            frame = clipboard_rx.recv() => {
                if let Some(frame) = frame {
                    write_secure_frame_writer(&mut writer, &frame).await?;
                    stats.record_frame(&frame);
                }
            },
            frame = receiver_rx.recv() => {
                let frame = frame.context("receiver frame reader ended")??;
                receiver_liveness.observe_authenticated_frame(tokio::time::Instant::now());
                match frame {
                    Frame::Heartbeat(heartbeat) => tracing::trace!(sequence = heartbeat.sequence, "heartbeat"),
                    Frame::Clipboard(event) => {
                        if let Err(error) = live_clipboard
                            .handle_remote_event(
                                event,
                                &runtime_config,
                                &mut writer,
                                &clipboard_tx,
                            )
                            .await
                        {
                            tracing::warn!(%error, "failed to synchronize receiver clipboard");
                            append_portable_log(log_path, format!("failed to synchronize receiver clipboard: {error}"));
                        }
                    }
                    Frame::ScreenInfo(info) => tracing::info!(primary = %info.primary_output, outputs = info.outputs.len(), "screen info"),
                    Frame::Control(control) => {
                        if control.role_epoch != INITIAL_ROLE_EPOCH {
                            tracing::debug!(
                                role_epoch = control.role_epoch,
                                "ignored receiver control from a stale role epoch"
                            );
                            continue;
                        }
                        match control.event {
                            ControlEvent::ReleaseToLocal { reason } => {
                                if edge_windows_input::handle_receiver_release(reason) {
                                    tracing::info!(?reason, "accepted receiver-requested local release");
                                    append_portable_log(log_path, format!("accepted receiver release: {reason:?}"));
                                } else {
                                    tracing::warn!(?reason, "ignored stale or implausible receiver release");
                                    append_portable_log(log_path, format!("ignored receiver release: {reason:?}"));
                                }
                            }
                            ControlEvent::SetInputForwarding { enabled } => {
                                if peer_supports_input_toggle {
                                    *input_forwarding_enabled = enabled;
                                    edge_windows_input::set_forwarding_enabled(enabled);
                                    update_windows_tray_input_forwarding(enabled, log_path);
                                    append_portable_log(
                                        log_path,
                                        format!("receiver set input forwarding to {enabled}"),
                                    );
                                }
                            }
                            event => tracing::debug!(?event, "receiver control frame"),
                        }
                    }
                    Frame::Error(err) => anyhow::bail!("receiver error: {}: {}", err.code, err.message),
                    Frame::Audio(AudioControl::Offer { udp_port, codecs }) => {
                        if audio_enabled && codecs.contains(&AudioCodec::PcmS16Stereo48Khz) {
                            update_windows_tray_audio(true, "Audio: Starting", log_path);
                            append_portable_log(log_path, format!("received Linux audio offer on UDP port {udp_port}"));
                            let secrets = SessionSecrets::generate();
                            let bind_addr = if peer_ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
                            match UdpSocket::bind(bind_addr).await {
                                Ok(socket) => {
                                    let windows_udp_port = socket.local_addr()?.port();
                                    write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Start {
                                        udp_port: windows_udp_port,
                                        session_id: secrets.session_id,
                                        session_salt: secrets.session_salt,
                                        session_key: secrets.session_key,
                                        codec: AudioCodec::PcmS16Stereo48Khz,
                                        frame_ms: edge_audio::FRAME_MS,
                                        jitter_target_ms: runtime_config.audio.jitter_target_ms as u16,
                                    })).await?;
                                    match edge_windows_audio::WindowsAudioReceiver::start(
                                        socket,
                                        std::net::SocketAddr::new(peer_ip, udp_port),
                                        secrets,
                                        runtime_config.audio.jitter_target_ms,
                                    ).await {
                                        Ok(receiver) => {
                                            _audio_receiver = Some(receiver);
                                            tracing::info!(udp_port, "started Linux audio receiver");
                                            append_portable_log(log_path, format!("opened Windows audio output on UDP port {windows_udp_port} and sent UDP probe"));
                                        }
                                        Err(error) => {
                                            tracing::warn!(%error, "failed to start Windows audio playback");
                                            append_portable_log(log_path, format!("failed to start Windows audio playback: {error:#}"));
                                            update_windows_tray_audio(true, &format!("Audio error: {error}"), log_path);
                                            write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::Stop { reason: AudioStopReason::PlaybackFailure })).await.ok();
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "failed to bind Windows audio UDP socket");
                                    append_portable_log(log_path, format!("failed to bind Windows audio UDP socket: {error}"));
                                    update_windows_tray_audio(true, &format!("Audio error: {error}"), log_path);
                                }
                            }
                        }
                    }
                    Frame::Audio(AudioControl::State { state, detail }) => {
                        tracing::info!(?state, ?detail, "Linux audio state changed");
                        append_portable_log(log_path, format!("Linux audio state changed: {state:?}{}", detail.as_deref().map(|detail| format!(": {detail}")).unwrap_or_default()));
                        if state == AudioStreamState::Streaming
                            && let Some(receiver) = &_audio_receiver
                        {
                            receiver.mark_linux_streaming();
                        }
                        let status = match state {
                            AudioStreamState::Disabled => "Audio: Off".to_string(),
                            AudioStreamState::WaitingForUdp | AudioStreamState::Starting => "Audio: Starting".to_string(),
                            AudioStreamState::Streaming => "Audio: Streaming".to_string(),
                            AudioStreamState::Error => format!("Audio error: {}", detail.as_deref().unwrap_or("unknown error")),
                        };
                        update_windows_tray_audio(audio_enabled, &status, log_path);
                    }
                    Frame::Audio(AudioControl::Stop { .. } | AudioControl::SetEnabled { enabled: false }) => {
                        audio_enabled = false;
                        _audio_receiver = None;
                        runtime_config.audio.enabled = false;
                        let mut updated = AppConfig::load_blocking(config_path).unwrap_or_else(|_| runtime_config.clone());
                        updated.audio.enabled = false;
                        if let Err(error) = updated.save_blocking(config_path) {
                            tracing::warn!(%error, "failed to persist Linux audio toggle");
                        }
                        update_windows_tray_audio(false, "Audio: Off", log_path);
                    }
                    Frame::Audio(AudioControl::SetEnabled { enabled: true }) => {
                        audio_enabled = true;
                        audio_restart_attempted = false;
                        runtime_config.audio.enabled = true;
                        let mut updated = AppConfig::load_blocking(config_path).unwrap_or_else(|_| runtime_config.clone());
                        updated.audio.enabled = true;
                        if let Err(error) = updated.save_blocking(config_path) {
                            tracing::warn!(%error, "failed to persist Linux audio toggle");
                        }
                        update_windows_tray_audio(true, "Audio: Starting", log_path);
                        if peer_supports_audio {
                            write_secure_frame_writer(&mut writer, &Frame::Audio(AudioControl::SetEnabled { enabled: true })).await?;
                        }
                    }
                    Frame::Audio(other) => tracing::debug!(?other, "audio control frame"),
                    other => tracing::debug!(?other, "receiver frame"),
                }
            },
            _ = clipboard_send.tick(), if live_clipboard.needs_send_tick() => {
                if live_clipboard.paste_barrier_expired() {
                    if let Some(cancel) = live_clipboard.cancel_expired_paste_barrier() {
                        write_secure_frame_writer(&mut writer, &cancel).await?;
                    }
                    tracing::warn!("clipboard image paste barrier timed out");
                    for held in live_clipboard.take_held_input() {
                        write_secure_frame_writer(&mut writer, &held).await?;
                        stats.record_frame(&held);
                        live_clipboard.after_input_sent(&held, &runtime_config, &clipboard_tx);
                    }
                    continue;
                }
                if let Some((frame, completed)) = live_clipboard.next_image_frame() {
                    write_secure_frame_writer(&mut writer, &frame).await?;
                    if completed {
                        stats.clipboard = stats.clipboard.saturating_add(1);
                        tracing::info!("completed Windows clipboard image transfer to receiver");
                        for held in live_clipboard.take_held_input() {
                            write_secure_frame_writer(&mut writer, &held).await?;
                            stats.record_frame(&held);
                            live_clipboard.after_input_sent(&held, &runtime_config, &clipboard_tx);
                        }
                    }
                }
            },
        }
    }
}

#[derive(Default)]
struct ControllerInputStats {
    frames: u64,
    motion: u64,
    buttons: u64,
    wheel: u64,
    keys: u64,
    clipboard: u64,
    control: u64,
}

impl ControllerInputStats {
    fn record_frame(&mut self, frame: &Frame) {
        self.frames = self.frames.saturating_add(1);
        match frame.input_event() {
            Some(InputEvent::PointerMotion { .. }) => {
                self.motion = self.motion.saturating_add(1);
            }
            Some(InputEvent::PointerButton { .. }) => {
                self.buttons = self.buttons.saturating_add(1);
            }
            Some(InputEvent::PointerWheel { .. }) => {
                self.wheel = self.wheel.saturating_add(1);
            }
            Some(InputEvent::Key { .. }) => {
                self.keys = self.keys.saturating_add(1);
            }
            Some(InputEvent::AllKeysUp) => {
                self.keys = self.keys.saturating_add(1);
            }
            None if matches!(frame, Frame::Clipboard(_)) => {
                self.clipboard = self.clipboard.saturating_add(1);
            }
            None if matches!(frame, Frame::Control(_)) => {
                self.control = self.control.saturating_add(1);
            }
            _ => {}
        }
    }

    fn log(&self, path: &Path, side: &str) {
        let capture = edge_windows_input::capture_stats();
        append_portable_log(
            path,
            format!(
                "{side} status frames={} motion={} buttons={} wheel={} keys={} clipboard={} control={} capture_active={} capture_suspended={} capture_mouse_hook_installed={} hook_mouse={} hook_keyboard={} raw_mouse={} raw_keyboard={} raw_input_repairs={} mouse_hook_repairs={} keyboard_hook_repairs={} input_pipeline_restarts={} callback_contention_drops={} input_supervisor_checks={} system_last_input_tick={} raw_worker_thread_id={} hook_worker_thread_id={} capture_input={} capture_control={} capture_enters={} capture_releases={} capture_return_edge_hits={} capture_game_guard_blocks={} capture_game_guard_releases={} capture_suspend_toggles={} capture_suspend_blocks={} capture_suspend_auto_resumes={} capture_send_failures={} capture_unmapped_keys={}",
                self.frames,
                self.motion,
                self.buttons,
                self.wheel,
                self.keys,
                self.clipboard,
                self.control,
                capture.active,
                capture.suspended,
                capture.mouse_hook_installed,
                capture.mouse_hook_events,
                capture.keyboard_hook_events,
                capture.raw_mouse_events,
                capture.raw_keyboard_events,
                capture.raw_input_repairs,
                capture.mouse_hook_repairs,
                capture.keyboard_hook_repairs,
                capture.input_pipeline_restarts,
                capture.callback_contention_drops,
                capture.input_supervisor_checks,
                capture.system_last_input_tick,
                capture.raw_worker_thread_id,
                capture.hook_worker_thread_id,
                capture.input_events,
                capture.control_events,
                capture.enter_events,
                capture.release_events,
                capture.return_edge_hits,
                capture.game_guard_blocks,
                capture.game_guard_releases,
                capture.suspend_toggles,
                capture.suspend_blocks,
                capture.suspend_auto_resumes,
                capture.send_failures,
                capture.unmapped_keys
            ),
        );
    }
}

fn spawn_receiver_reader(
    mut reader: TcpFrameReader,
) -> tokio::sync::mpsc::UnboundedReceiver<Result<Frame>> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let frame = read_secure_frame_reader(&mut reader)
                .await
                .context("failed to read receiver frame");
            let should_stop = frame.is_err();
            if sender.send(frame).is_err() || should_stop {
                break;
            }
        }
    });
    receiver
}

#[derive(Default)]
struct LiveClipboardState {
    #[cfg(windows)]
    ctrl_down: bool,
    #[cfg(windows)]
    tracker: ClipboardChangeTracker,
    #[cfg(windows)]
    last_clipboard_sequence: u32,
    #[cfg(windows)]
    outgoing: Option<OutgoingImageTransfer>,
    #[cfg(windows)]
    incoming: IncomingImageTransfer,
    #[cfg(windows)]
    next_transfer_id: u64,
    #[cfg(windows)]
    peer_supports_images: bool,
    #[cfg(windows)]
    outgoing_image_id: Option<ClipboardContentId>,
    #[cfg(windows)]
    last_sent_image_id: Option<ClipboardContentId>,
    #[cfg(windows)]
    held_input: VecDeque<Frame>,
    #[cfg(windows)]
    paste_barrier_deadline: Option<Instant>,
}

impl LiveClipboardState {
    async fn new(config: &AppConfig, peer_supports_images: bool) -> Result<Self> {
        #[cfg(windows)]
        {
            let last_observed = if config.clipboard.enabled {
                match edge_windows_input::read_clipboard_item(&config.clipboard).await {
                    Ok(item) => item.as_ref().map(ClipboardItem::id),
                    Err(error) => {
                        tracing::debug!(%error, "skipped initial Windows clipboard observation");
                        None
                    }
                }
            } else {
                None
            };
            Ok(Self {
                ctrl_down: false,
                tracker: ClipboardChangeTracker::new(last_observed),
                last_clipboard_sequence: edge_windows_input::clipboard_sequence_number(),
                outgoing: None,
                incoming: IncomingImageTransfer::default(),
                next_transfer_id: 0,
                peer_supports_images,
                outgoing_image_id: None,
                last_sent_image_id: None,
                held_input: VecDeque::new(),
                paste_barrier_deadline: None,
            })
        }

        #[cfg(not(windows))]
        {
            let _ = (config, peer_supports_images);
            Ok(Self::default())
        }
    }

    async fn prepare_input(&mut self, frame: Frame, config: &AppConfig) -> Result<Vec<Frame>> {
        #[cfg(windows)]
        {
            if self.paste_barrier_deadline.is_some() {
                // Pointer motion carries no clipboard ordering constraint, so let it
                // through instead of freezing the cursor for the length of the
                // transfer. Keys, buttons, and wheel stay queued: releasing those
                // ahead of the held paste could move focus and land it elsewhere.
                if matches!(frame.input_event(), Some(InputEvent::PointerMotion { .. })) {
                    return Ok(vec![frame]);
                }
                self.held_input.push_back(frame);
                return Ok(Vec::new());
            }
            if !config.clipboard.enabled {
                return Ok(vec![frame]);
            }

            if matches!(
                frame.input_event(),
                Some(InputEvent::Key {
                    evdev_code: 47,
                    down: true
                })
            ) && self.ctrl_down
            {
                let current = match edge_windows_input::read_clipboard_item(&config.clipboard).await
                {
                    Ok(current) => current,
                    Err(error) => {
                        tracing::debug!(%error, "could not prepare Windows clipboard before paste");
                        return Ok(vec![frame]);
                    }
                };
                if let Some(ClipboardItem::Image(image)) = &current
                    && self.peer_supports_images
                    && config.clipboard.images_enabled
                {
                    let image_id = ClipboardContentId::Image(image.content_sha256);
                    if self.last_sent_image_id != Some(image_id) {
                        let prefixes = if self.outgoing_image_id == Some(image_id) {
                            Vec::new()
                        } else {
                            self.offer_item(config, current, true)?
                        };
                        self.held_input.push_back(frame);
                        self.paste_barrier_deadline =
                            Some(Instant::now() + CLIPBOARD_PASTE_BARRIER_TIMEOUT);
                        return Ok(prefixes);
                    }
                } else {
                    let mut frames = self.offer_item(config, current, true)?;
                    frames.push(frame);
                    return Ok(frames);
                }
            }
            Ok(vec![frame])
        }

        #[cfg(not(windows))]
        {
            let _ = (frame, config);
            Ok(Vec::new())
        }
    }

    fn after_input_sent(
        &mut self,
        frame: &Frame,
        config: &AppConfig,
        clipboard_tx: &mpsc::UnboundedSender<Frame>,
    ) {
        #[cfg(windows)]
        {
            match frame.input_event() {
                Some(InputEvent::Key { evdev_code, down }) => match *evdev_code {
                    29 | 97 => {
                        self.ctrl_down = *down;
                    }
                    46 if *down && self.ctrl_down && config.clipboard.enabled => {
                        let clipboard_tx = clipboard_tx.clone();
                        let request =
                            if self.peer_supports_images && config.clipboard.images_enabled {
                                ClipboardEvent::ContentRequest
                            } else {
                                ClipboardEvent::TextRequest
                            };
                        tokio::spawn(async move {
                            time::sleep(Duration::from_millis(200)).await;
                            let _ = clipboard_tx.send(Frame::Clipboard(request));
                        });
                    }
                    _ => {}
                },
                Some(InputEvent::AllKeysUp) => {
                    self.ctrl_down = false;
                }
                _ => {}
            }
        }

        let _ = (frame, config, clipboard_tx);
    }

    async fn handle_remote_event(
        &mut self,
        event: ClipboardEvent,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
        clipboard_tx: &mpsc::UnboundedSender<Frame>,
    ) -> Result<()> {
        #[cfg(windows)]
        {
            if !config.clipboard.enabled {
                tracing::debug!(
                    ?event,
                    "clipboard event ignored because clipboard sync is disabled"
                );
                return Ok(());
            }

            match event {
                ClipboardEvent::TextOffer { text, .. } => {
                    let remote = ClipboardItem::Text(text.clone());
                    if self
                        .prefer_newer_local_clipboard(remote.id(), config, writer)
                        .await?
                    {
                        return Ok(());
                    }
                    edge_windows_input::write_clipboard_text(&text, config.clipboard.max_bytes)
                        .context("failed to write Windows clipboard")?;
                    self.tracker.mark_observed(Some(remote.id()));
                    self.last_clipboard_sequence = edge_windows_input::clipboard_sequence_number();
                    tracing::info!("updated Windows clipboard from receiver");
                }
                ClipboardEvent::TextRequest => {
                    let text = edge_windows_input::read_clipboard_text(config.clipboard.max_bytes)
                        .context("failed to read Windows clipboard")?;
                    if let Some(text) = text {
                        let item = ClipboardItem::Text(text.clone());
                        let Some(sequence) = self.tracker.offer_current(Some(item.id())) else {
                            return Ok(());
                        };
                        let frame = Frame::Clipboard(ClipboardEvent::TextOffer { sequence, text });
                        write_secure_frame_writer(writer, &frame).await?;
                        tracing::info!("sent Windows clipboard to receiver");
                    }
                }
                ClipboardEvent::ContentRequest => {
                    for frame in self.local_clipboard_offer(config, true).await? {
                        write_secure_frame_writer(writer, &frame).await?;
                    }
                }
                event @ (ClipboardEvent::ImageStart { .. }
                | ClipboardEvent::ImageChunk { .. }
                | ClipboardEvent::ImageEnd { .. }
                | ClipboardEvent::ImageCancel { .. }) => {
                    let transfer_id = event.image_transfer_id();
                    let is_cancel = matches!(&event, ClipboardEvent::ImageCancel { .. });
                    if is_cancel
                        && self
                            .outgoing
                            .as_ref()
                            .is_some_and(|active| Some(active.transfer_id()) == transfer_id)
                    {
                        self.outgoing = None;
                        tracing::info!("receiver cancelled Windows clipboard image transfer");
                        for held in self.take_held_input() {
                            write_secure_frame_writer(writer, &held).await?;
                            self.after_input_sent(&held, config, clipboard_tx);
                        }
                        return Ok(());
                    }
                    if !self.peer_supports_images || !config.clipboard.images_enabled {
                        if let Some(transfer_id) = transfer_id.filter(|_| !is_cancel) {
                            write_secure_frame_writer(
                                writer,
                                &Frame::Clipboard(ClipboardEvent::ImageCancel {
                                    transfer_id,
                                    reason: ClipboardCancelReason::Rejected,
                                }),
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                    match self
                        .incoming
                        .handle(event, config.clipboard.max_image_bytes)
                    {
                        Ok(Some(image)) => {
                            let remote_id = ClipboardContentId::Image(image.content_sha256);
                            if self
                                .prefer_newer_local_clipboard(remote_id, config, writer)
                                .await?
                            {
                                return Ok(());
                            }
                            let (width, height, bytes) =
                                (image.width, image.height, image.png.len());
                            edge_windows_input::write_clipboard_image(image)
                                .await
                                .context("failed to write Windows image clipboard")?;
                            self.tracker.mark_observed(Some(remote_id));
                            self.last_clipboard_sequence =
                                edge_windows_input::clipboard_sequence_number();
                            tracing::info!(
                                width,
                                height,
                                bytes,
                                "updated Windows image clipboard from receiver"
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            self.incoming.clear();
                            if let Some(transfer_id) = transfer_id.filter(|_| !is_cancel) {
                                let reason = match &error {
                                    ClipboardError::EncodedTooLarge { .. }
                                    | ClipboardError::TooManyPixels { .. }
                                    | ClipboardError::InvalidDimensions => {
                                        ClipboardCancelReason::Rejected
                                    }
                                    _ => ClipboardCancelReason::Invalid,
                                };
                                write_secure_frame_writer(
                                    writer,
                                    &Frame::Clipboard(ClipboardEvent::ImageCancel {
                                        transfer_id,
                                        reason,
                                    }),
                                )
                                .await?;
                            }
                            tracing::warn!(%error, "rejected receiver clipboard image transfer");
                        }
                    }
                }
            }
        }

        #[cfg(not(windows))]
        {
            let _ = (config, writer, clipboard_tx);
            tracing::info!(?event, "clipboard event");
        }

        Ok(())
    }

    #[cfg(windows)]
    async fn local_clipboard_offer(
        &mut self,
        config: &AppConfig,
        force: bool,
    ) -> Result<Vec<Frame>> {
        let current = edge_windows_input::read_clipboard_item(&config.clipboard)
            .await
            .context("failed to read Windows clipboard")?;
        self.offer_item(config, current, force)
    }

    #[cfg(windows)]
    async fn local_change_offer(&mut self, config: &AppConfig) -> Result<Vec<Frame>> {
        if !config.clipboard.enabled {
            return Ok(Vec::new());
        }
        // GetClipboardSequenceNumber returns 0 when the process has no clipboard
        // access to the window station. Treating that as a real sequence would
        // match `last_clipboard_sequence` forever and silently stop syncing, so
        // fall back to reading the clipboard instead.
        let sequence = edge_windows_input::clipboard_sequence_number();
        if sequence != 0 && sequence == self.last_clipboard_sequence {
            return Ok(Vec::new());
        }
        let frames = self.local_clipboard_offer(config, false).await?;
        self.last_clipboard_sequence = sequence;
        Ok(frames)
    }

    #[cfg(not(windows))]
    async fn local_change_offer(&mut self, config: &AppConfig) -> Result<Vec<Frame>> {
        let _ = config;
        Ok(Vec::new())
    }

    #[cfg(windows)]
    async fn prefer_newer_local_clipboard(
        &mut self,
        remote_id: ClipboardContentId,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
    ) -> Result<bool> {
        let current = edge_windows_input::read_clipboard_item(&config.clipboard)
            .await
            .context("failed to read Windows clipboard")?;
        let current_id = current.as_ref().map(ClipboardItem::id);
        if current_id == Some(remote_id) {
            self.tracker.mark_observed(current_id);
            return Ok(true);
        }

        if current_id.is_some() && !self.tracker.is_observed(&current_id) {
            for frame in self.offer_item(config, current, true)? {
                write_secure_frame_writer(writer, &frame).await?;
            }
            tracing::info!("kept newer Windows clipboard and sent it to receiver");
            return Ok(true);
        }

        Ok(false)
    }

    #[cfg(windows)]
    fn offer_item(
        &mut self,
        config: &AppConfig,
        current: Option<ClipboardItem>,
        force: bool,
    ) -> Result<Vec<Frame>> {
        let current_id = current.as_ref().map(ClipboardItem::id);
        let sequence = if force {
            self.tracker.offer_current(current_id)
        } else {
            self.tracker.offer_if_changed(current_id)
        };
        let Some(sequence) = sequence else {
            return Ok(Vec::new());
        };
        let mut frames = Vec::new();
        match current {
            Some(ClipboardItem::Text(text)) => {
                if let Some(active) = self.outgoing.take() {
                    frames.push(Frame::Clipboard(
                        active.cancel_event(ClipboardCancelReason::Replaced),
                    ));
                }
                self.outgoing_image_id = None;
                frames.push(Frame::Clipboard(ClipboardEvent::TextOffer {
                    sequence,
                    text,
                }));
            }
            Some(ClipboardItem::Image(image))
                if self.peer_supports_images && config.clipboard.images_enabled =>
            {
                if let Some(active) = self.outgoing.take() {
                    frames.push(Frame::Clipboard(
                        active.cancel_event(ClipboardCancelReason::Replaced),
                    ));
                }
                self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
                self.outgoing = Some(OutgoingImageTransfer::new(
                    self.next_transfer_id,
                    sequence,
                    image,
                ));
                self.outgoing_image_id = current_id;
            }
            _ => {}
        }
        Ok(frames)
    }

    #[cfg(windows)]
    fn next_image_frame(&mut self) -> Option<(Frame, bool)> {
        let transfer = self.outgoing.as_mut()?;
        let event = transfer.next_event()?;
        let completed = matches!(event, ClipboardEvent::ImageEnd { .. });
        if completed {
            self.outgoing = None;
            self.last_sent_image_id = self.outgoing_image_id.take();
        }
        Some((Frame::Clipboard(event), completed))
    }

    #[cfg(windows)]
    fn take_held_input(&mut self) -> Vec<Frame> {
        self.paste_barrier_deadline = None;
        self.held_input.drain(..).collect()
    }

    #[cfg(windows)]
    fn cancel_expired_paste_barrier(&mut self) -> Option<Frame> {
        self.paste_barrier_deadline = None;
        self.outgoing_image_id = None;
        self.outgoing
            .take()
            .map(|active| Frame::Clipboard(active.cancel_event(ClipboardCancelReason::TimedOut)))
    }

    #[cfg(windows)]
    fn paste_barrier_expired(&self) -> bool {
        self.paste_barrier_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Whether the 2 ms send tick has anything to do. Keeping it disabled while
    /// idle avoids waking the runtime 500 times a second for a whole session.
    #[cfg(windows)]
    fn needs_send_tick(&self) -> bool {
        self.outgoing.is_some() || self.paste_barrier_deadline.is_some()
    }

    #[cfg(not(windows))]
    fn needs_send_tick(&self) -> bool {
        false
    }
}

#[cfg(windows)]
fn start_live_input(
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
) -> Result<Option<mpsc::Receiver<Frame>>> {
    let (remote_size, used_fallback_size) = remote_size(screen_info.as_ref());
    if used_fallback_size {
        tracing::warn!(
            width = remote_size.width,
            height = remote_size.height,
            "receiver did not provide usable screen info; enabling live edge capture with fallback geometry"
        );
    }
    let capture = edge_windows_input::start_capture(edge_windows_input::CaptureConfig {
        edge: peer_position_to_edge(config.layout.listener_position),
        remote_size,
        game_compatibility: config.input.capture.game_compatibility,
    })
    .context("failed to start Windows live input capture")?;
    let (sender, receiver) = mpsc::channel(LIVE_INPUT_QUEUE_CAPACITY);
    std::thread::spawn(move || {
        let mut pending_motion = PendingMotion::default();
        let mut last_motion_flush = Instant::now();
        loop {
            match capture.recv_timeout(LIVE_INPUT_FLUSH_INTERVAL) {
                Ok(event) => {
                    let frame = captured_input_to_frame(event);
                    if pending_motion.coalesce(&frame) {
                        if last_motion_flush.elapsed() >= LIVE_INPUT_FLUSH_INTERVAL {
                            if !pending_motion.flush_lossy(&sender) {
                                break;
                            }
                            last_motion_flush = Instant::now();
                        }
                        continue;
                    }
                    if !pending_motion.flush_lossy(&sender) || sender.blocking_send(frame).is_err()
                    {
                        break;
                    }
                    last_motion_flush = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !pending_motion.flush_lossy(&sender) {
                        break;
                    }
                    last_motion_flush = Instant::now();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = pending_motion.flush_lossy(&sender);
                    break;
                }
            }
        }
    });
    tracing::info!("live Windows edge capture enabled");
    Ok(Some(receiver))
}

#[cfg(not(windows))]
fn start_live_input(
    _config: &AppConfig,
    _screen_info: Option<ScreenInfo>,
) -> Result<Option<mpsc::Receiver<Frame>>> {
    Ok(None)
}

async fn recv_live_input(receiver: &mut Option<mpsc::Receiver<Frame>>) -> Option<Frame> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(windows)]
fn captured_input_to_frame(event: edge_windows_input::CapturedInput) -> Frame {
    match event {
        edge_windows_input::CapturedInput::Input(event) => Frame::input(INITIAL_ROLE_EPOCH, event),
        edge_windows_input::CapturedInput::Control(event) => {
            Frame::control(INITIAL_ROLE_EPOCH, event)
        }
    }
}

#[cfg(windows)]
#[derive(Default)]
struct PendingMotion {
    dx: f64,
    dy: f64,
}

#[cfg(windows)]
impl PendingMotion {
    fn coalesce(&mut self, frame: &Frame) -> bool {
        if let Some(InputEvent::PointerMotion { dx, dy }) = frame.input_event() {
            self.dx += dx;
            self.dy += dy;
            true
        } else {
            false
        }
    }

    fn flush_lossy(&mut self, sender: &mpsc::Sender<Frame>) -> bool {
        if self.dx == 0.0 && self.dy == 0.0 {
            return true;
        }
        let frame = Frame::input(
            INITIAL_ROLE_EPOCH,
            InputEvent::PointerMotion {
                dx: self.dx,
                dy: self.dy,
            },
        );
        self.dx = 0.0;
        self.dy = 0.0;
        match sender.try_send(frame) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

#[cfg(windows)]
fn remote_size(screen_info: Option<&ScreenInfo>) -> (Size, bool) {
    let output = screen_info.and_then(|info| {
        info.outputs
            .iter()
            .find(|output| output.name == info.primary_output)
            .or_else(|| info.outputs.first())
    });
    match output {
        Some(output) if output.width > 0 && output.height > 0 => (
            Size {
                width: output.width,
                height: output.height,
            },
            false,
        ),
        _ => (FALLBACK_REMOTE_SIZE, true),
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use edge_protocol::OutputInfo;

    use super::{FALLBACK_REMOTE_SIZE, ScreenInfo, remote_size};

    #[test]
    fn missing_screen_info_keeps_capture_enabled_with_fallback_geometry() {
        assert_eq!(remote_size(None), (FALLBACK_REMOTE_SIZE, true));
    }

    #[test]
    fn advertised_primary_output_geometry_is_used() {
        let info = ScreenInfo {
            primary_output: "DP-2".to_string(),
            outputs: vec![
                OutputInfo {
                    name: "DP-1".to_string(),
                    width: 1920,
                    height: 1080,
                    scale: 1.0,
                    x: 0,
                    y: 0,
                },
                OutputInfo {
                    name: "DP-2".to_string(),
                    width: 2560,
                    height: 1440,
                    scale: 1.0,
                    x: 1920,
                    y: 0,
                },
            ],
        };

        assert_eq!(
            remote_size(Some(&info)),
            (
                edge_geometry::Size {
                    width: 2560,
                    height: 1440
                },
                false
            )
        );
    }
}

fn peer_position_to_edge(position: PeerPosition) -> Edge {
    match position {
        PeerPosition::Left => Edge::Left,
        PeerPosition::Right => Edge::Right,
        PeerPosition::Top => Edge::Top,
        PeerPosition::Bottom => Edge::Bottom,
    }
}

async fn drain_for(duration: Duration, session: &mut NoiseSession<TcpStream>) {
    let deadline = time::sleep(duration);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => return,
            frame = read_secure_frame(session) => {
                match frame {
                    Ok(frame) => tracing::debug!(?frame, "receiver frame"),
                    Err(err) => {
                        tracing::debug!(%err, "stopped draining receiver frames");
                        return;
                    }
                }
            }
        }
    }
}

async fn write_secure_frame(session: &mut NoiseSession<TcpStream>, frame: &Frame) -> Result<()> {
    let payload = encode_frame(frame)?;
    session.write_packet(&payload).await?;
    Ok(())
}

async fn write_secure_frame_writer(writer: &mut ScheduledNoiseWriter, frame: &Frame) -> Result<()> {
    writer.write(frame).await?;
    Ok(())
}

async fn read_secure_frame(session: &mut NoiseSession<TcpStream>) -> Result<Frame> {
    let payload = session.read_packet().await?;
    Ok(decode_frame(&payload)?)
}

async fn read_secure_frame_reader(reader: &mut TcpFrameReader) -> Result<Frame> {
    Ok(reader.read().await?)
}

async fn load_or_create_config(path: &PathBuf) -> Result<AppConfig> {
    match AppConfig::load_migrating(path).await {
        Ok(config) => Ok(config),
        Err(edge_common::CommonError::ReadConfig { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            let config = AppConfig::controller_default();
            config
                .save(path)
                .await
                .with_context(|| format!("failed to write default config to {}", path.display()))?;
            Ok(config)
        }
        Err(err) => Err(err).with_context(|| format!("failed to load {}", path.display())),
    }
}

fn default_config_path() -> PathBuf {
    portable_config_path("controller.toml")
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn receiver_hello_must_match_noise_identity() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "Laptop".to_string(),
            role: Role::Receiver,
            public_key_fingerprint: "actual".to_string(),
            capabilities: Vec::new(),
            extensions: Vec::new(),
            node_capabilities: Vec::new(),
        };
        assert!(validate_receiver_hello(&hello, "actual").is_ok());
        assert!(validate_receiver_hello(&hello, "different").is_err());
    }

    #[tokio::test]
    async fn legacy_controller_config_is_migrated_with_audio_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.toml");
        tokio::fs::write(
            &path,
            r#"device_name = "Main PC"
role = "controller"

[peer.laptop]
host = "192.168.0.11"
port = 42420
position = "left"
"#,
        )
        .await
        .unwrap();

        let config = load_or_create_config(&path).await.unwrap();
        let migrated = tokio::fs::read_to_string(&path).await.unwrap();

        assert!(config.audio.enabled);
        assert!(migrated.contains("[audio]"));
        assert!(migrated.contains("enabled = true"));
    }

    #[tokio::test]
    async fn explicit_disabled_audio_preference_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.toml");
        let mut config = AppConfig::controller_default();
        config.audio.enabled = false;
        config.save(&path).await.unwrap();

        let loaded = load_or_create_config(&path).await.unwrap();

        assert!(!loaded.audio.enabled);
    }

    #[tokio::test]
    async fn legacy_clipboard_config_is_migrated_with_images_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.toml");
        tokio::fs::write(
            &path,
            r#"device_name = "Main PC"
role = "controller"

[clipboard]
enabled = true
text_only = true
max_bytes = 1048576
"#,
        )
        .await
        .unwrap();

        let config = load_or_create_config(&path).await.unwrap();
        let migrated = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(config.clipboard.images_enabled);
        assert!(migrated.contains("images_enabled = true"));
        assert!(!migrated.contains("text_only"));
    }

    #[tokio::test]
    async fn obsolete_text_only_key_is_dropped_without_re_enabling_images() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.toml");
        tokio::fs::write(
            &path,
            r#"device_name = "Main PC"
role = "controller"

[clipboard]
enabled = true
text_only = true
images_enabled = false
max_bytes = 1048576
max_image_bytes = 4194304

[audio]
enabled = false
local_playback = "redirect"
jitter_target_ms = 60
"#,
        )
        .await
        .unwrap();

        // A complete [audio] table means `text_only` is the only thing that can
        // trigger the rewrite.
        let config = load_or_create_config(&path).await.unwrap();
        let migrated = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!config.clipboard.images_enabled);
        assert!(!migrated.contains("text_only"));
        assert!(migrated.contains("images_enabled = false"));
    }

    #[tokio::test]
    async fn explicit_disabled_clipboard_images_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.toml");
        let mut config = AppConfig::controller_default();
        config.clipboard.images_enabled = false;
        config.save(&path).await.unwrap();
        let loaded = load_or_create_config(&path).await.unwrap();
        assert!(!loaded.clipboard.images_enabled);
    }

    #[cfg(windows)]
    fn test_clipboard_state(peer_supports_images: bool) -> LiveClipboardState {
        LiveClipboardState {
            ctrl_down: true,
            tracker: ClipboardChangeTracker::new(None),
            last_clipboard_sequence: 0,
            outgoing: None,
            incoming: IncomingImageTransfer::default(),
            next_transfer_id: 0,
            peer_supports_images,
            outgoing_image_id: None,
            last_sent_image_id: None,
            held_input: VecDeque::new(),
            paste_barrier_deadline: None,
        }
    }

    #[cfg(windows)]
    #[test]
    fn image_transfer_is_capability_gated() {
        let config = AppConfig::controller_default();
        let image =
            edge_clipboard::CanonicalImage::from_rgba(1, 1, vec![1, 2, 3, 255], 1024).unwrap();
        let mut state = test_clipboard_state(false);
        let frames = state
            .offer_item(&config, Some(ClipboardItem::Image(image)), true)
            .unwrap();
        assert!(frames.is_empty());
        assert!(state.outgoing.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn paste_barrier_releases_after_end_and_timeout() {
        let image =
            edge_clipboard::CanonicalImage::from_rgba(1, 1, vec![1, 2, 3, 255], 1024).unwrap();
        let image_id = ClipboardContentId::Image(image.content_sha256);
        let mut state = test_clipboard_state(true);
        state.outgoing = Some(OutgoingImageTransfer::new(1, 1, image));
        state.outgoing_image_id = Some(image_id);
        state.paste_barrier_deadline = Some(Instant::now() + Duration::from_secs(1));
        state.held_input.push_back(Frame::input(
            INITIAL_ROLE_EPOCH,
            InputEvent::Key {
                evdev_code: 47,
                down: true,
            },
        ));
        while let Some((_, completed)) = state.next_image_frame() {
            if completed {
                break;
            }
        }
        assert_eq!(state.last_sent_image_id, Some(image_id));
        assert_eq!(state.take_held_input().len(), 1);

        state.paste_barrier_deadline = Some(Instant::now() - Duration::from_millis(1));
        state.outgoing = Some(OutgoingImageTransfer::new(
            2,
            2,
            edge_clipboard::CanonicalImage::from_rgba(1, 1, vec![4, 5, 6, 255], 1024).unwrap(),
        ));
        assert!(state.paste_barrier_expired());
        assert!(matches!(
            state.cancel_expired_paste_barrier(),
            Some(Frame::Clipboard(ClipboardEvent::ImageCancel {
                reason: ClipboardCancelReason::TimedOut,
                ..
            }))
        ));
    }
}
