#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::{
    fs::OpenOptions,
    future,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use edge_audio::SessionSecrets;
use edge_clipboard::{
    ClipboardChangeTracker, ClipboardContentId, ClipboardError, ClipboardItem,
    IncomingImageTransfer, OutgoingImageTransfer,
};
use edge_common::{
    AppConfig, AudioLocalPlayback, AudioRoutePreference, Role, TransportMode, default_state_dir,
    detect_primary_local_ip, init_tracing, portable_config_path,
};
#[cfg(target_os = "linux")]
use edge_crypto::initiate_noise_session;
use edge_crypto::{
    IdentityKey, NoiseSession, PinStatus, PinStore, accept_noise_session, pairing_code,
};
#[cfg(any(target_os = "linux", test))]
use edge_geometry::local_restore_point;
use edge_geometry::{Point, Rect, normalized_perpendicular};
use edge_linux_input::{
    ClipboardChangeWatcher, HyprCursorPosition, HyprlandVirtualInputBackend, LibeiBackend,
    UinputBackend, hyprland_cursor_position, hyprland_screen_info, read_clipboard_item,
    read_clipboard_text, spawn_clipboard_change_watcher, write_clipboard_image,
    write_clipboard_text,
};
use edge_protocol::{
    AUDIO_ROUTE_EXTENSION, AudioCodec, AudioControl, AudioStreamState, CLIPBOARD_IMAGE_EXTENSION,
    ClipboardCancelReason, ClipboardEvent, ControlEvent, Edge, Frame, Heartbeat, Hello,
    INITIAL_ROLE_EPOCH, INPUT_TOGGLE_EXTENSION, InputEvent, NodeCapability, OutputInfo,
    PAIRING_CONFIRMATION_EXTENSION, PROTOCOL_VERSION, PairingEvent, ReleaseReason, RemoteError,
    RoleEvent, ScreenInfo, decode_frame, encode_frame,
};
#[cfg(target_os = "linux")]
use edge_runtime::{
    AudioRouteStore, CommittedAudioRoute, CommittedRole, InputDirectionCapabilities,
    InputEpochGate, LivenessConfig, LivenessEvent, LivenessTracker, RoleCoordinator, RoleDecision,
    RoleStore, select_initial_controller, validate_commit, validate_prepare,
};
use edge_runtime::{SecureFrameReader, SecureFrameSession, SecureFrameWriter};
use edge_ui::{PairingConfirmationInput, PairingUiState, SettingsUiInput, run_settings_window};
#[cfg(target_os = "linux")]
use socket2::SockRef;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
    time,
};

type TcpFrameReader = SecureFrameReader<ReadHalf<TcpStream>>;
type ScheduledNoiseWriter = SecureFrameWriter<WriteHalf<TcpStream>>;

const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const CONTROLLER_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const RETURN_EDGE_POLL_INTERVAL: Duration = Duration::from_millis(40);
const RETURN_EDGE_MARGIN: i32 = 12;
const RETURN_EDGE_ENTRY_GRACE: Duration = Duration::from_millis(350);
const RETURN_EDGE_CONFIRMATIONS: u8 = 2;
static SETTINGS_PROCESS_OPEN: AtomicBool = AtomicBool::new(false);

mod tray;
use tray::{ControllerChoice, ReceiverTrayHandle, TrayCommand};

#[derive(Debug, Parser)]
#[command(version, about = "Linux edge-kvm node")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, help = "Arm two-sided confirmation for the next pairing")]
    pair: bool,
    #[arg(long)]
    test_input: Option<TestInput>,
    #[arg(
        long,
        value_enum,
        num_args = 0..=1,
        default_missing_value = "left",
        help = "Capture local input at an edge without connecting to a peer"
    )]
    test_capture: Option<TestCaptureEdge>,
    #[arg(long)]
    test_clipboard: bool,
    #[arg(long, help = "Exercise and restore the Linux audio routing path")]
    test_audio_route: bool,
    #[arg(long, help = "Disable the StatusNotifier tray item")]
    no_tray: bool,
    #[arg(long, hide = true)]
    settings: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TestInput {
    Pointer,
    Click,
    Wheel,
    Key,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TestCaptureEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<TestCaptureEdge> for Edge {
    fn from(edge: TestCaptureEdge) -> Self {
        match edge {
            TestCaptureEdge::Left => Self::Left,
            TestCaptureEdge::Right => Self::Right,
            TestCaptureEdge::Top => Self::Top,
            TestCaptureEdge::Bottom => Self::Bottom,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let receiver_log = default_state_dir().join("receiver.log");
    install_receiver_panic_log(receiver_log.clone());
    append_portable_log(&receiver_log, "receiver process starting");

    let result = tokio::select! {
        result = run_main(receiver_log.clone()) => result,
        signal = shutdown_signal() => {
            match signal {
                Ok(signal) => {
                    tracing::info!(signal, "receiver shutdown signal received");
                    append_portable_log(
                        &receiver_log,
                        format!("receiver shutdown signal received: {signal}"),
                    );
                    Ok(())
                }
                Err(err) => Err(err).context("failed to install receiver shutdown signal handler"),
            }
        }
    };
    match &result {
        Ok(()) => append_portable_log(&receiver_log, "receiver process exited cleanly"),
        Err(err) => append_portable_log(
            &receiver_log,
            format!("receiver process exited with error: {err:#}"),
        ),
    }
    result
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<&'static str> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    tokio::select! {
        _ = interrupt.recv() => Ok("SIGINT"),
        _ = terminate.recv() => Ok("SIGTERM"),
        _ = hangup.recv() => Ok("SIGHUP"),
    }
}

async fn run_main(receiver_log: PathBuf) -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(default_config_path);
    let config = load_or_create_config(&config_path).await?;

    if let Err(error) = edge_linux_audio::recover_portable_routing(&default_state_dir()).await {
        tracing::warn!(%error, "failed to recover previous Linux audio routing");
        append_portable_log(
            &receiver_log,
            format!("failed to recover previous Linux audio routing: {error:#}"),
        );
    }

    if args.settings {
        let settings_input = SettingsUiInput {
            role: config.preferred_role,
            config_path,
            config,
            local_ip: detect_primary_local_ip(),
            pairing: PairingUiState::Idle,
        };
        tokio::task::spawn_blocking(move || run_settings_window(settings_input))
            .await
            .context("settings window task failed")??;
        return Ok(());
    }

    if args.test_audio_route {
        edge_linux_audio::test_audio_route(&default_state_dir()).await?;
        tracing::info!("Linux audio route test completed and restored");
        return Ok(());
    }

    if let Some(edge) = args.test_capture {
        run_capture_test(edge.into()).await?;
        return Ok(());
    }

    if let Some(test) = args.test_input {
        let backend = ReceiverBackend::from_config(&config)?;
        run_input_test(&backend, test).await?;
        return Ok(());
    }

    if args.test_clipboard {
        let text = read_clipboard_text(&config.clipboard)
            .await?
            .unwrap_or_default();
        println!("{text}");
        return Ok(());
    }

    match config.transport {
        TransportMode::Listen => {
            let backend = ReceiverBackend::from_config(&config)?;
            run_receiver(
                config,
                config_path,
                args.pair,
                backend,
                !args.no_tray,
                receiver_log,
            )
            .await
        }
        TransportMode::Connect => {
            run_linux_connector(config, config_path, args.pair, !args.no_tray, receiver_log).await
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CaptureCounts {
    activations: u64,
    pointer_motion: u64,
    buttons: u64,
    wheel: u64,
    keyboard: u64,
    all_keys_up: u64,
    deactivations: u64,
    layout_changes: u64,
}

#[cfg(target_os = "linux")]
async fn run_capture_test(edge: Edge) -> Result<()> {
    use edge_linux_input::{CaptureEvent, PortalCaptureBackend};

    let mut backend = PortalCaptureBackend::preflight(edge)
        .await
        .context("InputCapture portal preflight failed")?;
    backend
        .arm()
        .await
        .context("failed to arm the InputCapture portal")?;

    println!(
        "Capture armed on the {edge:?} edge (zone set {}). Cross the edge to test; Ctrl+Alt+Pause releases locally; Ctrl+C exits.",
        backend.zone_set()
    );
    let mut counts = CaptureCounts::default();
    let mut displayed = counts;
    let mut status_tick = time::interval(Duration::from_secs(1));
    status_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let outcome = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = status_tick.tick() => {
                if counts != displayed {
                    println!(
                        "events: activation={} motion={} button={} wheel={} keyboard={} release={} deactivation={} layout={}",
                        counts.activations,
                        counts.pointer_motion,
                        counts.buttons,
                        counts.wheel,
                        counts.keyboard,
                        counts.all_keys_up,
                        counts.deactivations,
                        counts.layout_changes,
                    );
                    displayed = counts;
                }
            }
            event = backend.next_event() => {
                match event {
                    Some(CaptureEvent::Activated { .. }) => counts.activations += 1,
                    Some(CaptureEvent::Input(InputEvent::PointerMotion { .. })) => {
                        counts.pointer_motion += 1;
                    }
                    Some(CaptureEvent::Input(InputEvent::PointerButton { .. })) => {
                        counts.buttons += 1;
                    }
                    Some(CaptureEvent::Input(InputEvent::PointerWheel { .. })) => {
                        counts.wheel += 1;
                    }
                    Some(CaptureEvent::Input(InputEvent::Key { .. })) => {
                        counts.keyboard += 1;
                    }
                    Some(CaptureEvent::Input(InputEvent::AllKeysUp)) => {
                        counts.all_keys_up += 1;
                    }
                    Some(CaptureEvent::Deactivated) => counts.deactivations += 1,
                    Some(CaptureEvent::EmergencyReleased) => {
                        println!("Emergency release accepted locally; capture is no longer active.");
                        break Ok(());
                    }
                    Some(CaptureEvent::LayoutChanged { .. }) => {
                        counts.layout_changes += 1;
                        if let Err(error) = backend.arm().await {
                            break Err(anyhow::Error::new(error)
                                .context("layout changed and capture could not be re-armed"));
                        }
                    }
                    Some(CaptureEvent::BackendFailed(error)) => {
                        break Err(anyhow::anyhow!("capture backend failed: {error}"));
                    }
                    None => break Err(anyhow::anyhow!("capture backend stopped unexpectedly")),
                }
            }
        }
    };

    let cleanup = backend.disarm().await;
    match (outcome, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(anyhow::Error::new(error).context("failed to disarm capture")),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_capture_test(_edge: Edge) -> Result<()> {
    anyhow::bail!("--test-capture is available only on Linux")
}

async fn load_or_create_config(path: &PathBuf) -> Result<AppConfig> {
    match AppConfig::load_migrating(path).await {
        Ok(config) => Ok(config),
        Err(edge_common::CommonError::ReadConfig { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            let config = AppConfig::receiver_default();
            config
                .save(path)
                .await
                .with_context(|| format!("failed to write default config to {}", path.display()))?;
            Ok(config)
        }
        Err(err) => Err(err).with_context(|| format!("failed to load {}", path.display())),
    }
}

async fn run_input_test(backend: &ReceiverBackend, test: TestInput) -> Result<()> {
    match test {
        TestInput::Pointer => {
            backend
                .inject(InputEvent::PointerMotion { dx: 50.0, dy: 0.0 })
                .await?
        }
        TestInput::Click => {
            backend
                .inject(InputEvent::PointerButton {
                    button: edge_protocol::MouseButton::Left,
                    down: true,
                })
                .await?;
            backend
                .inject(InputEvent::PointerButton {
                    button: edge_protocol::MouseButton::Left,
                    down: false,
                })
                .await?;
        }
        TestInput::Wheel => {
            backend
                .inject(InputEvent::PointerWheel { x: 0.0, y: -1.0 })
                .await?;
        }
        TestInput::Key => {
            backend
                .inject(InputEvent::Key {
                    evdev_code: 30,
                    down: true,
                })
                .await?;
            backend
                .inject(InputEvent::Key {
                    evdev_code: 30,
                    down: false,
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct LinuxPeerConnection {
    session: NoiseSession<TcpStream>,
    local_name: String,
    local_fingerprint: String,
    peer_name: String,
    peer_fingerprint: String,
    peer_ip: std::net::IpAddr,
    initial_role_state: edge_protocol::RoleState,
    local_screen_info: Option<ScreenInfo>,
    peer_screen_info: Option<ScreenInfo>,
    peer_supports_input_capture: bool,
    peer_supports_input_injection: bool,
    peer_supports_role_switch: bool,
    peer_supports_audio_capture: bool,
    peer_supports_audio_playback: bool,
    peer_supports_audio_route: bool,
    peer_supports_images: bool,
}

#[cfg(target_os = "linux")]
async fn run_linux_connector(
    mut config: AppConfig,
    config_path: PathBuf,
    initial_pairing_armed: bool,
    enable_tray: bool,
    log_path: PathBuf,
) -> Result<()> {
    let identity = IdentityKey::load_or_create(default_state_dir().join("identity.toml"))
        .await
        .context("failed to load Linux connector identity")?;
    let mut pairing_armed = initial_pairing_armed;
    let endpoint = format!("{}:{}", config.peer.host, config.peer.port);
    let (tray, mut tray_commands) = if enable_tray {
        let (tray, commands) = ReceiverTrayHandle::spawn(
            format!("Connect: {endpoint}"),
            "InputCapture portal".to_string(),
            pairing_armed,
        )
        .await
        .context("failed to start Linux tray")?;
        (Some(tray), Some(commands))
    } else {
        (None, None)
    };
    let mut connection_enabled = true;
    let connector_pause = Arc::new(AtomicBool::new(false));

    loop {
        if !connection_enabled {
            let command = recv_tray_command(&mut tray_commands).await;
            match command {
                TrayCommandEvent::Command(TrayCommand::Reconnect) => {
                    connection_enabled = true;
                    continue;
                }
                TrayCommandEvent::Command(TrayCommand::ArmPairing) => {
                    pairing_armed = true;
                    connection_enabled = true;
                    if let Some(tray) = &tray {
                        tray.pairing_armed(true).await;
                    }
                    continue;
                }
                TrayCommandEvent::Command(TrayCommand::OpenSettings) => {
                    open_receiver_settings(&config_path, &log_path);
                }
                TrayCommandEvent::Command(TrayCommand::Quit) => break,
                TrayCommandEvent::Command(
                    TrayCommand::Disconnect
                    | TrayCommand::ToggleInputForwarding
                    | TrayCommand::SetAudio(_)
                    | TrayCommand::SetController(_),
                )
                | TrayCommandEvent::Closed => {}
            }
            continue;
        }

        if let Some(tray) = &tray {
            tray.connecting().await;
        }
        let connection = connect_linux_peer(
            &config,
            &config_path,
            &identity,
            pairing_armed,
            connector_pause.load(Ordering::Acquire),
            &log_path,
        )
        .await;
        let connection = match connection {
            Ok(connection) => {
                pairing_armed = false;
                config.peer.pinned_fingerprint = connection.peer_fingerprint.clone();
                if let Some(tray) = &tray {
                    tray.pairing_armed(false).await;
                    tray.connected(format!(
                        "{} ({})",
                        connection.peer_name, connection.peer_fingerprint
                    ))
                    .await;
                }
                connection
            }
            Err(error) => {
                let message = error.to_string();
                append_portable_log(&log_path, format!("Linux connector failed: {error:#}"));
                if let Some(tray) = &tray {
                    tray.error(message.clone()).await;
                }
                if message.contains("Upgrade the other computer") {
                    connection_enabled = false;
                    continue;
                }
                tokio::select! {
                    _ = time::sleep(Duration::from_secs(2)) => {}
                    command = recv_tray_command(&mut tray_commands) => {
                        match command {
                            TrayCommandEvent::Command(TrayCommand::Quit) => break,
                            TrayCommandEvent::Command(TrayCommand::Disconnect) => {
                                connection_enabled = false;
                            }
                            TrayCommandEvent::Command(TrayCommand::ArmPairing) => {
                                pairing_armed = true;
                                if let Some(tray) = &tray {
                                    tray.pairing_armed(true).await;
                                }
                            }
                            TrayCommandEvent::Command(TrayCommand::OpenSettings) => {
                                open_receiver_settings(&config_path, &log_path);
                            }
                            TrayCommandEvent::Command(
                                TrayCommand::Reconnect
                                | TrayCommand::ToggleInputForwarding
                                | TrayCommand::SetAudio(_)
                                | TrayCommand::SetController(_),
                            )
                            | TrayCommandEvent::Closed => {}
                        }
                    }
                }
                continue;
            }
        };

        match run_linux_controller_session(
            connection,
            &config,
            &config_path,
            tray.as_ref(),
            &mut tray_commands,
            &connector_pause,
            &log_path,
        )
        .await
        {
            Ok(ControllerSessionExit::QuitRequested) => break,
            Ok(ControllerSessionExit::DisconnectRequested) => {
                connection_enabled = false;
                if let Some(tray) = &tray {
                    tray.disconnected_by_user().await;
                }
            }
            Err(error) => {
                append_portable_log(
                    &log_path,
                    format!("Linux controller session ended: {error:#}"),
                );
                if let Some(tray) = &tray {
                    tray.disconnected(Some(error.to_string())).await;
                }
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    if let Some(tray) = &tray {
        tray.shutdown().await;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn connect_linux_peer(
    config: &AppConfig,
    config_path: &Path,
    identity: &IdentityKey,
    pairing_armed: bool,
    session_paused: bool,
    log_path: &Path,
) -> Result<LinuxPeerConnection> {
    if config.peer.host.trim().is_empty() {
        anyhow::bail!("peer.host is required when transport = \"connect\"");
    }
    if config.peer.pinned_fingerprint.is_empty() && !pairing_armed {
        anyhow::bail!("peer is not paired; choose 'Pair or replace peer...' on both computers");
    }

    let endpoint = format!("{}:{}", config.peer.host, config.peer.port);
    let stream = time::timeout(Duration::from_secs(5), TcpStream::connect(&endpoint))
        .await
        .with_context(|| format!("connection to {endpoint} timed out"))?
        .with_context(|| format!("failed to connect to {endpoint}"))?;
    let peer_ip = stream.peer_addr()?.ip();
    configure_controller_socket(&stream)?;
    let expected_fingerprint = (!pairing_armed && !config.peer.pinned_fingerprint.is_empty())
        .then_some(config.peer.pinned_fingerprint.as_str());
    let (mut session, peer_fingerprint) = time::timeout(
        Duration::from_secs(15),
        initiate_noise_session(stream, identity, expected_fingerprint),
    )
    .await
    .context("encrypted handshake timed out")?
    .context("failed encrypted handshake with Linux peer")?;
    let locally_trusted = config.peer.pinned_fingerprint == peer_fingerprint;

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
                AUDIO_ROUTE_EXTENSION.to_string(),
            ],
            node_capabilities: vec![
                NodeCapability::InputCaptureV1,
                NodeCapability::InputInjectV1,
                NodeCapability::RoleSwitchV1,
                NodeCapability::ScreenInfoBothSidesV1,
                NodeCapability::AudioCaptureV1,
                NodeCapability::AudioPlaybackV1,
            ],
        }),
    )
    .await?;

    let hello = time::timeout(Duration::from_secs(15), async {
        loop {
            match read_secure_frame(&mut session).await? {
                Frame::Hello(hello) => break Ok(hello),
                Frame::Error(error) => {
                    anyhow::bail!("peer error: {}: {}", error.code, error.message)
                }
                frame => tracing::debug!(?frame, "waiting for peer hello"),
            }
        }
    })
    .await
    .context("timed out waiting for peer hello")??;
    validate_controller_hello(&hello, &peer_fingerprint)?;

    let supports_confirmation = hello
        .extensions
        .iter()
        .any(|extension| extension == PAIRING_CONFIRMATION_EXTENSION);
    if supports_confirmation {
        write_secure_frame(
            &mut session,
            &Frame::Pairing(PairingEvent::Status {
                trusted: locally_trusted,
                armed: pairing_armed,
            }),
        )
        .await?;
        let (peer_trusted, peer_armed) = read_pairing_status(&mut session).await?;
        if !locally_trusted || !peer_trusted {
            if !pairing_armed || !peer_armed {
                write_secure_frame(
                    &mut session,
                    &Frame::Pairing(PairingEvent::Decision { accepted: false }),
                )
                .await
                .ok();
                anyhow::bail!("pairing needs approval on both computers");
            }
            let confirmation = PairingConfirmationInput {
                peer_name: hello.device_name.clone(),
                peer_addr: Some(endpoint.clone()),
                local_fingerprint: identity.fingerprint(),
                peer_fingerprint: peer_fingerprint.clone(),
                verification_code: pairing_code(&identity.fingerprint(), &peer_fingerprint),
                previous_peer_fingerprint: (!config.peer.pinned_fingerprint.is_empty()
                    && config.peer.pinned_fingerprint != peer_fingerprint)
                    .then(|| config.peer.pinned_fingerprint.clone()),
            };
            let accepted = tokio::task::spawn_blocking(move || {
                edge_ui::run_pairing_confirmation(confirmation)
            })
            .await
            .context("Linux pairing confirmation task failed")??;
            write_secure_frame(
                &mut session,
                &Frame::Pairing(PairingEvent::Decision { accepted }),
            )
            .await?;
            if !accepted || !read_pairing_decision(&mut session).await? {
                anyhow::bail!("pairing was not approved on both computers");
            }
            let mut updated = AppConfig::load(config_path)
                .await
                .unwrap_or_else(|_| config.clone());
            updated.peer.pinned_fingerprint = peer_fingerprint.clone();
            updated.save(config_path).await?;
            append_portable_log(
                log_path,
                format!("paired peer {} ({peer_fingerprint})", hello.device_name),
            );
        }
    } else if !locally_trusted {
        anyhow::bail!("peer does not support two-sided pairing confirmation; update it first");
    }

    let local_fingerprint = identity.fingerprint();
    let peer_supports_input_capture = hello
        .node_capabilities
        .contains(&NodeCapability::InputCaptureV1);
    let peer_supports_input_injection = hello
        .node_capabilities
        .contains(&NodeCapability::InputInjectV1);
    let peer_supports_role_switch = hello
        .node_capabilities
        .contains(&NodeCapability::RoleSwitchV1);
    let peer_supports_audio_capture = hello
        .node_capabilities
        .contains(&NodeCapability::AudioCaptureV1);
    let peer_supports_audio_playback = hello
        .node_capabilities
        .contains(&NodeCapability::AudioPlaybackV1);
    let peer_supports_audio_route = hello
        .extensions
        .iter()
        .any(|extension| extension == AUDIO_ROUTE_EXTENSION);
    let role_store = RoleStore::new(default_state_dir().join("role.toml"));
    let persisted = role_store.load().await.unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to read committed role state; using fresh-pair selection");
        None
    });
    let persisted_controller = persisted
        .filter(|role| role.belongs_to(&local_fingerprint, &peer_fingerprint))
        .map(|role| role.controller_fingerprint);
    let selected_controller = persisted_controller.or_else(|| {
        select_initial_controller(
            &local_fingerprint,
            &peer_fingerprint,
            config.preferred_role == Role::Controller,
            InputDirectionCapabilities {
                controller_can_capture: true,
                receiver_can_inject: peer_supports_input_injection,
            },
            InputDirectionCapabilities {
                controller_can_capture: peer_supports_input_capture,
                receiver_can_inject: true,
            },
        )
        .map(str::to_string)
    });
    if let Some(controller) = &selected_controller {
        role_store
            .save(&CommittedRole::new(controller))
            .await
            .context("failed to persist initial controller assignment")?;
    } else {
        role_store.clear().await.ok();
    }
    let initial_role_state = edge_protocol::RoleState {
        controller_fingerprint: selected_controller,
        role_epoch: INITIAL_ROLE_EPOCH,
        transition: edge_protocol::RoleTransitionState::Stable,
        listener_position: peer_position_to_edge(config.layout.listener_position),
        paused: session_paused,
        failure_detail: None,
    };
    write_secure_frame(
        &mut session,
        &Frame::Role(RoleEvent::SessionState(initial_role_state.clone())),
    )
    .await?;

    let requested_output = linux_capture_output(config);
    let local_screen_info = match hyprland_screen_info(requested_output).await {
        Ok(info) => {
            write_secure_frame(&mut session, &Frame::ScreenInfo(info.clone())).await?;
            Some(info)
        }
        Err(error) => {
            tracing::warn!(%error, "failed to query local Linux screen geometry");
            None
        }
    };
    let peer_screen_info = read_initial_peer_screen(&mut session).await?;
    let peer_supports_images = hello
        .extensions
        .iter()
        .any(|extension| extension == CLIPBOARD_IMAGE_EXTENSION);
    Ok(LinuxPeerConnection {
        session,
        local_name: config.device_name.clone(),
        local_fingerprint,
        peer_name: hello.device_name,
        peer_fingerprint,
        peer_ip,
        initial_role_state,
        local_screen_info,
        peer_screen_info,
        peer_supports_input_capture,
        peer_supports_input_injection,
        peer_supports_role_switch,
        peer_supports_audio_capture,
        peer_supports_audio_playback,
        peer_supports_audio_route,
        peer_supports_images,
    })
}

#[cfg(target_os = "linux")]
async fn read_initial_peer_screen(
    session: &mut NoiseSession<TcpStream>,
) -> Result<Option<ScreenInfo>> {
    time::timeout(Duration::from_secs(15), async {
        loop {
            match read_secure_frame(session).await? {
                Frame::ScreenInfo(info) => break Ok(Some(info)),
                Frame::Heartbeat(_) => break Ok(None),
                Frame::Error(error) => {
                    anyhow::bail!("peer error: {}: {}", error.code, error.message)
                }
                frame => tracing::debug!(?frame, "waiting for peer screen info"),
            }
        }
    })
    .await
    .context("timed out waiting for peer screen info")?
}

#[cfg(target_os = "linux")]
async fn run_linux_controller_session(
    connection: LinuxPeerConnection,
    config: &AppConfig,
    config_path: &Path,
    tray: Option<&ReceiverTrayHandle>,
    tray_commands: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
    connector_pause: &AtomicBool,
    log_path: &Path,
) -> Result<ControllerSessionExit> {
    use edge_linux_input::{CaptureEvent, PortalCaptureBackend};

    let LinuxPeerConnection {
        session,
        local_name,
        local_fingerprint,
        peer_name,
        peer_fingerprint,
        peer_ip,
        initial_role_state,
        local_screen_info,
        peer_screen_info,
        peer_supports_input_capture,
        peer_supports_input_injection,
        peer_supports_role_switch,
        peer_supports_audio_capture,
        peer_supports_audio_playback,
        peer_supports_audio_route,
        peer_supports_images,
    } = connection;
    if let Some(info) = &peer_screen_info {
        tracing::info!(
            primary = %info.primary_output,
            outputs = info.outputs.len(),
            "Linux node screen info"
        );
    }

    let edge = initial_role_state.listener_position;
    let injector = ReceiverBackend::from_config(config)
        .context("failed to prepare Linux connector input injection")?;
    let mut coordinator = RoleCoordinator::new(
        local_fingerprint.clone(),
        peer_fingerprint.clone(),
        initial_role_state.controller_fingerprint.clone(),
        initial_role_state.role_epoch,
        edge,
        initial_role_state.paused,
    )?;
    let mut role_epoch = initial_role_state.role_epoch;
    let mut local_is_controller =
        initial_role_state.controller_fingerprint.as_deref() == Some(local_fingerprint.as_str());
    let mut session_paused = initial_role_state.paused;
    let role_switch_available =
        peer_supports_role_switch && peer_supports_input_capture && peer_supports_input_injection;
    let mut input_forwarding_enabled = initial_role_state.controller_fingerprint.is_some();
    let mut capture = if local_is_controller && peer_supports_input_injection {
        let backend = PortalCaptureBackend::preflight(edge)
            .await
            .context("Linux controller capture preflight failed")?;
        if input_forwarding_enabled && !session_paused {
            backend
                .arm()
                .await
                .context("failed to arm Linux controller capture")?;
        }
        Some(backend)
    } else {
        None
    };
    let mut return_watcher = RemoteReturnWatcher::new(local_screen_info.clone());
    let mut input_epoch = InputEpochGate::default();
    let mut pending_readiness: Option<ConnectorRoleReadiness> = None;
    let mut transition_deadline: Option<tokio::time::Instant> = None;
    let role_store = RoleStore::new(default_state_dir().join("role.toml"));
    let audio_route_store = AudioRouteStore::new(default_state_dir().join("audio.toml"));
    let stored_audio_route = audio_route_store
        .load()
        .await
        .context("failed to load committed audio route")?
        .filter(|route| route.belongs_to(&local_fingerprint, &peer_fingerprint));
    let mut audio_source = match stored_audio_route.as_ref() {
        Some(route) => route.source_fingerprint.clone(),
        None => match config.audio.route {
            Some(AudioRoutePreference::Disabled) => None,
            Some(AudioRoutePreference::LocalToPeer) => Some(local_fingerprint.clone()),
            Some(AudioRoutePreference::PeerToLocal) => Some(peer_fingerprint.clone()),
            None if config.audio.enabled => Some(peer_fingerprint.clone()),
            None => None,
        },
    };
    if audio_source.as_deref() == Some(local_fingerprint.as_str())
        && (!peer_supports_audio_playback || !peer_supports_audio_route)
        || audio_source.as_deref() == Some(peer_fingerprint.as_str())
            && !peer_supports_audio_capture
    {
        audio_source = None;
    }
    if stored_audio_route.is_none() {
        let committed = audio_source.clone().map_or_else(
            CommittedAudioRoute::disabled,
            CommittedAudioRoute::from_source,
        );
        audio_route_store.save(&committed).await?;
    }
    let audio_socket = Arc::new(
        UdpSocket::bind(if peer_ip.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?,
    );
    let mut audio_sender: Option<edge_linux_audio::LinuxAudioSender> = None;
    let mut audio_receiver: Option<edge_linux_audio::LinuxAudioReceiver> = None;
    let (audio_start_tx, mut audio_start_rx) = mpsc::unbounded_channel::<AudioStartResult>();
    let mut audio_start_task: Option<AbortOnDropTask> = None;
    let mut audio_start_generation = 0_u64;
    if let Some(tray) = tray {
        tray.input_forwarding(input_forwarding_enabled).await;
        tray.audio_route(
            linux_audio_choice(&audio_source, &local_fingerprint, &peer_fingerprint),
            peer_supports_audio_playback && peer_supports_audio_route,
            peer_supports_audio_capture,
        )
        .await;
        tray.session_paused(session_paused).await;
        tray.role_assignment(
            local_name.clone(),
            peer_name.clone(),
            local_is_controller,
            role_switch_available && !session_paused,
        )
        .await;
    }

    let (reader, mut writer) = SecureFrameSession::new(session).split();
    send_linux_audio_route(
        &mut writer,
        peer_supports_audio_route,
        &audio_source,
        &peer_fingerprint,
    )
    .await?;
    if audio_source.as_deref() == Some(local_fingerprint.as_str()) && !session_paused {
        send_linux_audio_offer(&mut writer, &audio_socket).await?;
    }
    let mut frame_rx = spawn_controller_reader(reader);
    let mut clipboard_sync = ReceiverClipboardState::new(config, peer_supports_images).await?;
    let mut clipboard_watcher = config
        .clipboard
        .enabled
        .then(spawn_clipboard_change_watcher);
    let mut clipboard_send = time::interval(Duration::from_millis(2));
    clipboard_send.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let liveness_config = LivenessConfig::default();
    let mut peer_liveness = LivenessTracker::new(liveness_config, tokio::time::Instant::now());
    let mut watchdog = time::interval(Duration::from_millis(250));
    watchdog.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let heartbeat = time::sleep(liveness_config.heartbeat_interval(false));
    tokio::pin!(heartbeat);
    let mut heartbeat_sequence = 0_u64;
    let mut input_active = false;

    let outcome: Result<ControllerSessionExit> = async {
        loop {
            if !session_paused && writer.bulk_is_due() && clipboard_sync.outgoing.is_some() {
                clipboard_sync.send_next_image_frame(&mut writer).await?;
                continue;
            }

            tokio::select! {
                biased;
                _ = watchdog.tick() => {
                    if transition_deadline.is_some_and(|deadline| {
                        tokio::time::Instant::now() >= deadline
                    }) && coordinator.is_transitioning() {
                        transition_deadline = None;
                        pending_readiness = None;
                        let abort = coordinator.abort("peer did not complete role preflight in time")?;
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Role(RoleEvent::Abort(abort)),
                        )
                        .await
                        .ok();
                        if local_is_controller
                            && input_forwarding_enabled
                            && !session_paused
                            && let Some(capture) = &capture
                        {
                            capture.arm().await.ok();
                        }
                        if let Some(tray) = tray {
                            tray.role_failure("Role switch timed out".to_string()).await;
                        }
                    }
                    match peer_liveness.poll(tokio::time::Instant::now(), input_active) {
                        Some(LivenessEvent::SoftInputTimeout) => {
                            if local_is_controller && let Some(capture) = &capture {
                                capture.release(None).await.ok();
                            } else {
                                input_epoch.suspend();
                                injector.all_keys_up().await.ok();
                            }
                            input_active = false;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            tracing::warn!("Linux peer was silent; released active input direction");
                        }
                        Some(LivenessEvent::HardSessionTimeout) => {
                            anyhow::bail!("Linux peer stopped responding for five seconds");
                        }
                        None => {}
                    }
                    if audio_sender.as_ref().is_some_and(|sender| sender.is_finished()) {
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Error,
                                detail: Some("Linux audio capture stopped unexpectedly".to_string()),
                            }),
                        ).await.ok();
                    }
                    if audio_receiver.as_ref().is_some_and(|receiver| receiver.is_finished())
                        && let Some(receiver) = audio_receiver.take()
                    {
                        let failure = receiver.failure_reason().await;
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Error,
                                detail: Some(failure),
                            }),
                        ).await.ok();
                    }
                }
                _ = &mut heartbeat => {
                    heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                    write_secure_frame_writer(
                        &mut writer,
                        &Frame::Heartbeat(Heartbeat { sequence: heartbeat_sequence }),
                    )
                    .await?;
                    heartbeat.as_mut().reset(
                        tokio::time::Instant::now()
                            + liveness_config.heartbeat_interval(
                                input_active || coordinator.is_transitioning(),
                            ),
                    );
                }
                event = recv_portal_capture_event(&mut capture) => {
                    match event {
                        Some(CaptureEvent::Activated {
                            edge,
                            normalized_position,
                            ..
                        }) => {
                            if session_paused || !local_is_controller || !input_forwarding_enabled {
                                continue;
                            }
                            input_active = true;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::control(
                                    role_epoch,
                                    ControlEvent::EnterRemote {
                                        edge,
                                        normalized_position,
                                    },
                                ),
                            )
                            .await?;
                        }
                        Some(CaptureEvent::Input(event)) => {
                            if session_paused || !local_is_controller || !input_forwarding_enabled {
                                continue;
                            }
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, event),
                            )
                            .await?;
                            if let Some(tray) = tray {
                                tray.input_event().await;
                            }
                        }
                        Some(CaptureEvent::Deactivated) => {
                            input_active = false;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, InputEvent::AllKeysUp),
                            )
                            .await?;
                        }
                        Some(CaptureEvent::EmergencyReleased) => {
                            input_active = false;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            tracing::info!("Linux emergency release chord restored local input");
                        }
                        Some(CaptureEvent::LayoutChanged { .. }) => {
                            input_active = false;
                            if coordinator.is_transitioning() {
                                let abort = coordinator.abort(
                                    "monitor layout changed during capture preflight",
                                )?;
                                pending_readiness = None;
                                transition_deadline = None;
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::Role(RoleEvent::Abort(abort)),
                                )
                                .await?;
                                if let Some(tray) = tray {
                                    tray.role_switching(false).await;
                                }
                            }
                            if local_is_controller
                                && input_forwarding_enabled
                                && !session_paused
                                && let Some(capture) = &capture
                            {
                                capture.arm().await.context(
                                    "monitor layout changed and capture could not be re-armed",
                                )?;
                            }
                        }
                        Some(CaptureEvent::BackendFailed(error)) => {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                            anyhow::bail!("Linux capture backend failed: {error}");
                        }
                        None if capture.is_some() => {
                            anyhow::bail!("Linux capture backend stopped unexpectedly");
                        }
                        None => {}
                    }
                }
                Some(started) = audio_start_rx.recv(), if audio_start_task.is_some() => {
                    if started.generation != audio_start_generation {
                        if let Ok((_, sender)) = started.result {
                            sender.stop().await.ok();
                        }
                        continue;
                    }
                    audio_start_task = None;
                    match started.result {
                        Ok((destination, sender)) => {
                            audio_sender = Some(sender);
                            append_portable_log(log_path, format!("authenticated peer audio UDP endpoint: {destination}"));
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::State {
                                    state: AudioStreamState::Streaming,
                                    detail: None,
                                }),
                            ).await?;
                        }
                        Err(error) => {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::State {
                                    state: AudioStreamState::Error,
                                    detail: Some(error),
                                }),
                            ).await?;
                        }
                    }
                }
                command = recv_tray_command(tray_commands) => {
                    match command {
                        TrayCommandEvent::Command(TrayCommand::OpenSettings) => {
                            open_receiver_settings(config_path, log_path);
                        }
                        TrayCommandEvent::Command(TrayCommand::ToggleInputForwarding) => {
                            if !session_paused
                                && !coordinator.is_transitioning()
                                && coordinator.state().controller_fingerprint.is_some()
                            {
                                input_forwarding_enabled = !input_forwarding_enabled;
                                input_active = false;
                                if local_is_controller && let Some(capture) = &capture {
                                    if input_forwarding_enabled {
                                        capture.arm().await?;
                                    } else {
                                        capture.disarm().await?;
                                    }
                                } else if !input_forwarding_enabled {
                                    injector.all_keys_up().await.ok();
                                }
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::control(
                                        role_epoch,
                                        ControlEvent::SetInputForwarding {
                                            enabled: input_forwarding_enabled,
                                        },
                                    ),
                                )
                                .await?;
                                if let Some(tray) = tray {
                                    tray.input_forwarding(input_forwarding_enabled).await;
                                }
                            }
                        }
                        TrayCommandEvent::Command(TrayCommand::SetController(choice)) => {
                            let requested = match choice {
                                ControllerChoice::Local => local_fingerprint.as_str(),
                                ControllerChoice::Peer => peer_fingerprint.as_str(),
                            };
                            if session_paused || !role_switch_available {
                                tracing::warn!("role switch is unavailable for this peer");
                            } else if coordinator.state().controller_fingerprint.as_deref()
                                != Some(requested)
                                && !coordinator.is_transitioning()
                            {
                                input_epoch.suspend();
                                pending_readiness = begin_connector_role_switch(
                                    requested,
                                    &local_fingerprint,
                                    &mut coordinator,
                                    &mut capture,
                                    &injector,
                                    &mut writer,
                                    edge,
                                )
                                .await?;
                                transition_deadline = pending_readiness
                                    .as_ref()
                                    .map(|_| tokio::time::Instant::now() + Duration::from_secs(5));
                                input_active = false;
                                if let Some(tray) = tray {
                                    tray.role_switching(pending_readiness.is_some()).await;
                                }
                            }
                        }
                        TrayCommandEvent::Command(TrayCommand::SetAudio(choice)) => {
                            let requested_source = match choice {
                                tray::AudioChoice::Off => None,
                                tray::AudioChoice::Local if peer_supports_audio_playback && peer_supports_audio_route => Some(local_fingerprint.clone()),
                                tray::AudioChoice::Peer if peer_supports_audio_capture => Some(peer_fingerprint.clone()),
                                _ => {
                                    tracing::warn!(?choice, "requested audio direction is unavailable");
                                    continue;
                                }
                            };
                            audio_source = requested_source;
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            audio_receiver = None;
                            let committed = audio_source.clone().map_or_else(
                                CommittedAudioRoute::disabled,
                                CommittedAudioRoute::from_source,
                            );
                            audio_route_store.save(&committed).await?;
                            send_linux_audio_route(
                                &mut writer,
                                peer_supports_audio_route,
                                &audio_source,
                                &peer_fingerprint,
                            ).await?;
                            if audio_source.as_deref() == Some(local_fingerprint.as_str()) && !session_paused {
                                send_linux_audio_offer(&mut writer, &audio_socket).await?;
                            }
                            if let Some(tray) = tray {
                                tray.audio_route(
                                    choice,
                                    peer_supports_audio_playback && peer_supports_audio_route,
                                    peer_supports_audio_capture,
                                ).await;
                            }
                        }
                        TrayCommandEvent::Command(TrayCommand::Disconnect) => {
                            if !coordinator.is_transitioning() {
                                coordinator.set_paused(true)?;
                                session_paused = true;
                                connector_pause.store(true, Ordering::Release);
                                if let Some(capture) = &capture {
                                    capture.release(None).await.ok();
                                    capture.disarm().await.ok();
                                }
                                input_epoch.suspend();
                                injector.all_keys_up().await.ok();
                                drop(audio_start_task.take());
                                audio_start_generation = audio_start_generation.wrapping_add(1);
                                if let Some(sender) = audio_sender.take() {
                                    sender.stop().await.ok();
                                }
                                audio_receiver = None;
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::Audio(AudioControl::Stop {
                                        reason: edge_protocol::AudioStopReason::UserRequest,
                                    }),
                                ).await.ok();
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::Role(RoleEvent::SetPaused { paused: true }),
                                )
                                .await?;
                                if let Some(tray) = tray {
                                    tray.session_paused(true).await;
                                    tray.role_assignment(
                                        local_name.clone(),
                                        peer_name.clone(),
                                        local_is_controller,
                                        false,
                                    )
                                    .await;
                                }
                            }
                        }
                        TrayCommandEvent::Command(TrayCommand::Reconnect) => {
                            coordinator.set_paused(false)?;
                            session_paused = false;
                            connector_pause.store(false, Ordering::Release);
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::SetPaused { paused: false }),
                            )
                            .await?;
                            send_linux_audio_route(
                                &mut writer,
                                peer_supports_audio_route,
                                &audio_source,
                                &peer_fingerprint,
                            ).await?;
                            if audio_source.as_deref() == Some(local_fingerprint.as_str()) {
                                send_linux_audio_offer(&mut writer, &audio_socket).await?;
                            }
                            if local_is_controller
                                && input_forwarding_enabled
                                && let Some(capture) = &capture
                            {
                                capture.arm().await?;
                            }
                            if let Some(tray) = tray {
                                tray.session_paused(false).await;
                                tray.role_assignment(
                                    local_name.clone(),
                                    peer_name.clone(),
                                    local_is_controller,
                                    role_switch_available,
                                )
                                .await;
                            }
                        }
                        TrayCommandEvent::Command(TrayCommand::Quit) => {
                            break Ok(ControllerSessionExit::QuitRequested);
                        }
                        TrayCommandEvent::Command(
                            TrayCommand::ArmPairing,
                        )
                        | TrayCommandEvent::Closed => {}
                    }
                }
                event = recv_clipboard_change(&mut clipboard_watcher) => {
                    match event {
                        ClipboardWatchEvent::Changed => {
                            if !session_paused
                                && clipboard_sync.send_changed_offer(config, &mut writer).await?
                                && let Some(tray) = tray
                            {
                                tray.clipboard_event().await;
                            }
                        }
                        ClipboardWatchEvent::Closed => {
                            clipboard_watcher = Some(spawn_clipboard_change_watcher());
                        }
                    }
                }
                frame = frame_rx.recv() => {
                    let frame = frame.context("Linux peer frame reader ended")??;
                    peer_liveness.observe_authenticated_frame(tokio::time::Instant::now());
                    writer.record_received(&frame);
                    match frame {
                        Frame::Heartbeat(_) => {}
                        Frame::Clipboard(event) => {
                            if !session_paused
                                && clipboard_sync.handle_event(config, &mut writer, event).await?
                                && let Some(tray) = tray
                            {
                                tray.clipboard_event().await;
                            }
                        }
                        Frame::Input(input)
                            if input.role_epoch == role_epoch
                                && !local_is_controller
                                && input_forwarding_enabled
                                && !session_paused =>
                        {
                            if input.event == InputEvent::AllKeysUp {
                                input_epoch.suspend();
                                injector.all_keys_up().await?;
                                continue;
                            }
                            if !input_epoch.accepts_input() {
                                continue;
                            }
                            let is_motion = matches!(input.event, InputEvent::PointerMotion { .. });
                            injector.inject(input.event).await?;
                            if is_motion
                                && let Some(control) = return_watcher.release_if_at_edge().await?
                            {
                                input_epoch.suspend();
                                injector.all_keys_up().await.ok();
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::control(role_epoch, control),
                                )
                                .await?;
                            }
                            if let Some(tray) = tray {
                                tray.input_event().await;
                            }
                        }
                        Frame::Input(input) => {
                            tracing::trace!(
                                role_epoch = input.role_epoch,
                                "ignored input outside the committed receiving epoch"
                            );
                        }
                        Frame::Control(control) if control.role_epoch == role_epoch => {
                            match control.event {
                                ControlEvent::LeaveRemote {
                                    edge,
                                    normalized_position,
                                } if local_is_controller => {
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::input(role_epoch, InputEvent::AllKeysUp),
                                    )
                                    .await?;
                                    if let Some(capture) = &capture {
                                        let cursor = linux_release_cursor(
                                            local_screen_info.as_ref(),
                                            edge,
                                            normalized_position,
                                        );
                                        capture.release(cursor).await?;
                                    }
                                    input_active = false;
                                }
                                ControlEvent::ReleaseToLocal { .. } if local_is_controller => {
                                    if let Some(capture) = &capture {
                                        capture.release(None).await?;
                                    }
                                    input_active = false;
                                }
                                ControlEvent::EnterRemote { edge, normalized_position }
                                    if !local_is_controller =>
                                {
                                    input_epoch.observe_control(&ControlEvent::EnterRemote {
                                        edge,
                                        normalized_position,
                                    });
                                    return_watcher.record_control(&ControlEvent::EnterRemote {
                                        edge,
                                        normalized_position,
                                    });
                                }
                                ControlEvent::SetInputForwarding { enabled } => {
                                    input_forwarding_enabled = enabled;
                                    input_active = false;
                                    if local_is_controller && let Some(capture) = &capture {
                                        if enabled {
                                            if !session_paused {
                                                capture.arm().await?;
                                            }
                                        } else {
                                            capture.disarm().await?;
                                        }
                                    } else if !enabled {
                                        input_epoch.suspend();
                                        injector.all_keys_up().await.ok();
                                    }
                                    if let Some(tray) = tray {
                                        tray.input_forwarding(enabled).await;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Frame::Control(control) => {
                            tracing::debug!(
                                role_epoch = control.role_epoch,
                                "ignored stale Linux peer control"
                            );
                        }
                        Frame::Role(RoleEvent::Request { controller_fingerprint }) => {
                            if !session_paused
                                && role_switch_available
                                && (controller_fingerprint == local_fingerprint
                                    || controller_fingerprint == peer_fingerprint)
                                && coordinator.state().controller_fingerprint.as_deref()
                                    != Some(controller_fingerprint.as_str())
                                && !coordinator.is_transitioning()
                            {
                                input_epoch.suspend();
                                pending_readiness = begin_connector_role_switch(
                                    &controller_fingerprint,
                                    &local_fingerprint,
                                    &mut coordinator,
                                    &mut capture,
                                    &injector,
                                    &mut writer,
                                    edge,
                                )
                                .await?;
                                transition_deadline = pending_readiness
                                    .as_ref()
                                    .map(|_| tokio::time::Instant::now() + Duration::from_secs(5));
                                input_active = false;
                                if let Some(tray) = tray {
                                    tray.role_switching(pending_readiness.is_some()).await;
                                }
                            }
                        }
                        Frame::Role(RoleEvent::Ready {
                            role_epoch: ready_epoch,
                            capture_ready,
                            inject_ready,
                            failure_detail,
                        }) => {
                            let Some(local_ready) = pending_readiness.take() else {
                                tracing::debug!(ready_epoch, "ignored unexpected role readiness");
                                continue;
                            };
                            transition_deadline = None;
                            let readiness_failure = failure_detail
                                .or_else(|| local_ready.failure_detail.clone());
                            let decision = match coordinator.finish_ready(
                                ready_epoch,
                                local_ready.capture_ready,
                                local_ready.inject_ready,
                                capture_ready,
                                inject_ready,
                                readiness_failure,
                            ) {
                                Ok(decision) => decision,
                                Err(edge_runtime::RoleStateError::UnexpectedEpoch { .. }) => {
                                    pending_readiness = Some(local_ready);
                                    transition_deadline = Some(
                                        tokio::time::Instant::now() + Duration::from_secs(5),
                                    );
                                    tracing::debug!(ready_epoch, "ignored stale role readiness");
                                    continue;
                                }
                                Err(error) => return Err(error.into()),
                            };
                            match decision {
                                RoleDecision::Commit(commit) => {
                                    let controller = commit.controller_fingerprint.as_deref()
                                        .context("role commit omitted controller identity")?;
                                    role_store
                                        .save(&CommittedRole::new(controller))
                                        .await
                                        .context("failed to persist committed role before handover")?;
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::Role(RoleEvent::Commit(commit.clone())),
                                    )
                                    .await?;
                                    role_epoch = commit.role_epoch;
                                    local_is_controller = controller == local_fingerprint;
                                    input_epoch.suspend();
                                    injector.all_keys_up().await.ok();
                                    if local_is_controller
                                        && input_forwarding_enabled
                                        && !session_paused
                                        && let Some(capture) = &capture
                                    {
                                        capture.arm().await?;
                                    }
                                    if let Some(tray) = tray {
                                        tray.role_assignment(
                                            local_name.clone(),
                                            peer_name.clone(),
                                            local_is_controller,
                                            role_switch_available && !session_paused,
                                        )
                                        .await;
                                    }
                                }
                                RoleDecision::Abort(abort) => {
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::Role(RoleEvent::Abort(abort.clone())),
                                    )
                                    .await?;
                                    if local_is_controller
                                        && input_forwarding_enabled
                                        && !session_paused
                                        && let Some(capture) = &capture
                                    {
                                        capture.arm().await?;
                                    }
                                    if let Some(tray) = tray {
                                        tray.role_switching(false).await;
                                        if let Some(detail) = abort.failure_detail {
                                            tray.role_failure(detail).await;
                                        }
                                    }
                                }
                            }
                        }
                        Frame::Error(error) => {
                            anyhow::bail!("Linux peer error: {}: {}", error.code, error.message);
                        }
                        Frame::ScreenInfo(info) => {
                            tracing::info!(primary = %info.primary_output, "updated Linux peer screen info");
                        }
                        Frame::Role(
                            RoleEvent::SessionState(_)
                            | RoleEvent::Prepare(_)
                            | RoleEvent::Commit(_)
                            | RoleEvent::Abort(_),
                        ) => {
                            tracing::warn!("ignored connector-authoritative role message from listener");
                        }
                        Frame::Role(RoleEvent::SetPaused { paused }) => {
                            if coordinator.is_transitioning() {
                                tracing::warn!("ignored pause request during role handover");
                                continue;
                            }
                            coordinator.set_paused(paused)?;
                            session_paused = paused;
                            connector_pause.store(paused, Ordering::Release);
                            if paused {
                                if let Some(capture) = &capture {
                                    capture.release(None).await.ok();
                                    capture.disarm().await.ok();
                                }
                                input_epoch.suspend();
                                injector.all_keys_up().await.ok();
                                drop(audio_start_task.take());
                                audio_start_generation = audio_start_generation.wrapping_add(1);
                                if let Some(sender) = audio_sender.take() {
                                    sender.stop().await.ok();
                                }
                                audio_receiver = None;
                            } else if local_is_controller
                                && input_forwarding_enabled
                                && let Some(capture) = &capture
                            {
                                capture.arm().await?;
                            }
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::SetPaused { paused }),
                            )
                            .await?;
                            if !paused {
                                send_linux_audio_route(
                                    &mut writer,
                                    peer_supports_audio_route,
                                    &audio_source,
                                    &peer_fingerprint,
                                ).await?;
                                if audio_source.as_deref() == Some(local_fingerprint.as_str()) {
                                    send_linux_audio_offer(&mut writer, &audio_socket).await?;
                                }
                            }
                            if let Some(tray) = tray {
                                tray.session_paused(paused).await;
                                tray.role_assignment(
                                    local_name.clone(),
                                    peer_name.clone(),
                                    local_is_controller,
                                    role_switch_available && !paused,
                                )
                                .await;
                            }
                        }
                        Frame::Audio(AudioControl::RequestRoute { source_fingerprint }) => {
                            let valid = source_fingerprint.as_deref().is_none_or(|source| {
                                source == local_fingerprint && peer_supports_audio_playback && peer_supports_audio_route
                                    || source == peer_fingerprint && peer_supports_audio_capture
                            });
                            if valid {
                                audio_source = source_fingerprint;
                                drop(audio_start_task.take());
                                audio_start_generation = audio_start_generation.wrapping_add(1);
                                if let Some(sender) = audio_sender.take() {
                                    sender.stop().await.ok();
                                }
                                audio_receiver = None;
                                let committed = audio_source.clone().map_or_else(
                                    CommittedAudioRoute::disabled,
                                    CommittedAudioRoute::from_source,
                                );
                                audio_route_store.save(&committed).await?;
                                send_linux_audio_route(
                                    &mut writer,
                                    peer_supports_audio_route,
                                    &audio_source,
                                    &peer_fingerprint,
                                ).await?;
                                if audio_source.as_deref() == Some(local_fingerprint.as_str()) && !session_paused {
                                    send_linux_audio_offer(&mut writer, &audio_socket).await?;
                                }
                                if let Some(tray) = tray {
                                    tray.audio_route(
                                        linux_audio_choice(&audio_source, &local_fingerprint, &peer_fingerprint),
                                        peer_supports_audio_playback && peer_supports_audio_route,
                                        peer_supports_audio_capture,
                                    ).await;
                                }
                            } else {
                                send_linux_audio_route(
                                    &mut writer,
                                    peer_supports_audio_route,
                                    &audio_source,
                                    &peer_fingerprint,
                                ).await?;
                            }
                        }
                        Frame::Audio(AudioControl::Offer { udp_port, codecs }) => {
                            if audio_source.as_deref() != Some(peer_fingerprint.as_str())
                                || !codecs.contains(&AudioCodec::PcmS16Stereo48Khz)
                            {
                                continue;
                            }
                            let secrets = SessionSecrets::generate();
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::Start {
                                    udp_port: audio_socket.local_addr()?.port(),
                                    session_id: secrets.session_id,
                                    session_salt: secrets.session_salt,
                                    session_key: secrets.session_key,
                                    codec: AudioCodec::PcmS16Stereo48Khz,
                                    frame_ms: edge_audio::FRAME_MS,
                                    jitter_target_ms: config.audio.jitter_target_ms as u16,
                                }),
                            ).await?;
                            match edge_linux_audio::LinuxAudioReceiver::start(
                                audio_socket.clone(),
                                std::net::SocketAddr::new(peer_ip, udp_port),
                                secrets,
                                config.audio.jitter_target_ms,
                            ).await {
                                Ok(receiver) => {
                                    audio_receiver = Some(receiver);
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::Audio(AudioControl::State {
                                            state: AudioStreamState::Streaming,
                                            detail: None,
                                        }),
                                    ).await?;
                                }
                                Err(error) => {
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::Audio(AudioControl::State {
                                            state: AudioStreamState::Error,
                                            detail: Some(error.to_string()),
                                        }),
                                    ).await?;
                                }
                            }
                        }
                        Frame::Audio(AudioControl::Start {
                            udp_port,
                            session_id,
                            session_salt,
                            session_key,
                            codec,
                            frame_ms,
                            ..
                        }) => {
                            if audio_source.as_deref() != Some(local_fingerprint.as_str())
                                || udp_port == 0
                                || codec != AudioCodec::PcmS16Stereo48Khz
                                || frame_ms != edge_audio::FRAME_MS
                            {
                                continue;
                            }
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            let generation = audio_start_generation;
                            let secrets = SessionSecrets { session_id, session_salt, session_key };
                            let advertised_destination = std::net::SocketAddr::new(peer_ip, udp_port);
                            let redirect = config.audio.local_playback == AudioLocalPlayback::Redirect;
                            let start_socket = audio_socket.clone();
                            let state_dir = default_state_dir();
                            let result_tx = audio_start_tx.clone();
                            audio_start_task = Some(AbortOnDropTask(tokio::spawn(async move {
                                let result = async {
                                    let cipher = edge_audio::PacketCipher::new(&secrets);
                                    let destination = edge_linux_audio::establish_peer(
                                        &start_socket,
                                        &cipher,
                                        advertised_destination,
                                        peer_ip,
                                        Duration::from_secs(3),
                                    ).await?;
                                    let sender = edge_linux_audio::LinuxAudioSender::start(
                                        start_socket,
                                        destination,
                                        secrets,
                                        &state_dir,
                                        redirect,
                                    ).await?;
                                    Ok::<_, anyhow::Error>((destination, sender))
                                }.await.map_err(|error| format!("{error:#}"));
                                let _ = result_tx.send(AudioStartResult { generation, result });
                            })));
                        }
                        Frame::Audio(AudioControl::Stop { .. }) => {
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            audio_receiver = None;
                        }
                        Frame::Audio(AudioControl::SetEnabled { enabled }) => {
                            audio_source = enabled.then(|| peer_fingerprint.clone());
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            audio_receiver = None;
                            let committed = audio_source.clone().map_or_else(
                                CommittedAudioRoute::disabled,
                                CommittedAudioRoute::from_source,
                            );
                            audio_route_store.save(&committed).await?;
                            send_linux_audio_route(
                                &mut writer,
                                peer_supports_audio_route,
                                &audio_source,
                                &peer_fingerprint,
                            ).await?;
                            if let Some(tray) = tray {
                                tray.audio_route(
                                    linux_audio_choice(&audio_source, &local_fingerprint, &peer_fingerprint),
                                    peer_supports_audio_playback && peer_supports_audio_route,
                                    peer_supports_audio_capture,
                                ).await;
                            }
                        }
                        Frame::Audio(
                            AudioControl::SetRoute { .. }
                            | AudioControl::State { .. }
                        ) => {}
                        Frame::Hello(_) | Frame::Pairing(_) => {}
                    }
                }
                _ = clipboard_send.tick(), if !session_paused && clipboard_sync.outgoing.is_some() => {
                    clipboard_sync.send_next_image_frame(&mut writer).await?;
                }
            }
        }
    }
    .await;

    write_secure_frame_writer(
        &mut writer,
        &Frame::input(role_epoch, InputEvent::AllKeysUp),
    )
    .await
    .ok();
    if let Some(capture) = &capture {
        capture.release(None).await.ok();
        capture.disarm().await.ok();
    }
    drop(audio_start_task.take());
    if let Some(sender) = audio_sender.take() {
        sender.stop().await.ok();
    }
    drop(audio_receiver.take());
    injector.all_keys_up().await.ok();
    outcome
}

#[cfg(target_os = "linux")]
struct ConnectorRoleReadiness {
    capture_ready: bool,
    inject_ready: bool,
    failure_detail: Option<String>,
}

#[cfg(target_os = "linux")]
async fn begin_connector_role_switch(
    requested_controller: &str,
    local_fingerprint: &str,
    coordinator: &mut RoleCoordinator,
    capture: &mut Option<edge_linux_input::PortalCaptureBackend>,
    injector: &ReceiverBackend,
    writer: &mut ScheduledNoiseWriter,
    local_exit_edge: Edge,
) -> Result<Option<ConnectorRoleReadiness>> {
    let old_local_controller =
        coordinator.state().controller_fingerprint.as_deref() == Some(local_fingerprint);
    if old_local_controller {
        if let Some(capture) = capture.as_ref() {
            capture.release(None).await.ok();
            capture.disarm().await.ok();
        }
        write_secure_frame_writer(
            writer,
            &Frame::input(coordinator.state().role_epoch, InputEvent::AllKeysUp),
        )
        .await
        .ok();
    } else {
        injector.all_keys_up().await.ok();
    }

    let prepare = coordinator.prepare(requested_controller)?;
    write_secure_frame_writer(writer, &Frame::Role(RoleEvent::Prepare(prepare))).await?;

    let mut readiness = ConnectorRoleReadiness {
        capture_ready: true,
        inject_ready: true,
        failure_detail: None,
    };
    if requested_controller == local_fingerprint && capture.is_none() {
        match edge_linux_input::PortalCaptureBackend::preflight(local_exit_edge).await {
            Ok(backend) => *capture = Some(backend),
            Err(error) => {
                readiness.capture_ready = false;
                readiness.failure_detail = Some(format!("capture preflight failed: {error}"));
            }
        }
    }
    Ok(Some(readiness))
}

#[cfg(target_os = "linux")]
fn linux_audio_choice(
    source: &Option<String>,
    local_fingerprint: &str,
    peer_fingerprint: &str,
) -> tray::AudioChoice {
    match source.as_deref() {
        Some(source) if source == local_fingerprint => tray::AudioChoice::Local,
        Some(source) if source == peer_fingerprint => tray::AudioChoice::Peer,
        _ => tray::AudioChoice::Off,
    }
}

#[cfg(target_os = "linux")]
async fn send_linux_audio_offer(
    writer: &mut ScheduledNoiseWriter,
    socket: &UdpSocket,
) -> Result<()> {
    write_secure_frame_writer(
        writer,
        &Frame::Audio(AudioControl::Offer {
            udp_port: socket.local_addr()?.port(),
            codecs: vec![AudioCodec::PcmS16Stereo48Khz],
        }),
    )
    .await
}

#[cfg(target_os = "linux")]
async fn send_linux_audio_route(
    writer: &mut ScheduledNoiseWriter,
    peer_supports_route: bool,
    source: &Option<String>,
    peer_fingerprint: &str,
) -> Result<()> {
    let control = if peer_supports_route {
        AudioControl::SetRoute {
            source_fingerprint: source.clone(),
        }
    } else {
        AudioControl::SetEnabled {
            enabled: source.as_deref() == Some(peer_fingerprint),
        }
    };
    write_secure_frame_writer(writer, &Frame::Audio(control)).await
}

#[cfg(target_os = "linux")]
async fn recv_portal_capture_event(
    capture: &mut Option<edge_linux_input::PortalCaptureBackend>,
) -> Option<edge_linux_input::CaptureEvent> {
    match capture {
        Some(capture) => capture.next_event().await,
        None => future::pending().await,
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_release_cursor(
    screen_info: Option<&ScreenInfo>,
    edge: Edge,
    normalized_position: f32,
) -> Option<(f64, f64)> {
    let info = screen_info?;
    let output = info
        .outputs
        .iter()
        .find(|output| output.name == info.primary_output)
        .or_else(|| info.outputs.first())?;
    let point = local_restore_point(
        edge,
        normalized_position,
        Rect {
            x: f64::from(output.x),
            y: f64::from(output.y),
            width: output.width,
            height: output.height,
        },
        3.0,
    );
    Some((point.x, point.y))
}

#[cfg(any(target_os = "linux", test))]
fn linux_capture_output(config: &AppConfig) -> &str {
    if config.input.capture.output.trim().is_empty() {
        config.input.inject.output.as_str()
    } else {
        config.input.capture.output.as_str()
    }
}

#[cfg(any(target_os = "linux", test))]
fn peer_position_to_edge(position: edge_common::PeerPosition) -> Edge {
    match position {
        edge_common::PeerPosition::Left => Edge::Left,
        edge_common::PeerPosition::Right => Edge::Right,
        edge_common::PeerPosition::Top => Edge::Top,
        edge_common::PeerPosition::Bottom => Edge::Bottom,
    }
}

fn opposite_edge(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_linux_connector(
    _config: AppConfig,
    _config_path: PathBuf,
    _initial_pairing_armed: bool,
    _enable_tray: bool,
    _log_path: PathBuf,
) -> Result<()> {
    anyhow::bail!("Linux controller mode is available only on Linux")
}

async fn run_receiver(
    config: AppConfig,
    config_path: PathBuf,
    initial_pairing_armed: bool,
    backend: ReceiverBackend,
    enable_tray: bool,
    log_path: PathBuf,
) -> Result<()> {
    let state_dir = default_state_dir();
    let identity = IdentityKey::load_or_create(state_dir.join("identity.toml"))
        .await
        .context("failed to load receiver identity")?;
    let mut pins = PinStore::load_or_default(state_dir.join("pins.toml"))
        .await
        .context("failed to load pin store")?;

    let listen = config
        .listen
        .clone()
        .unwrap_or_else(|| "0.0.0.0:42420".to_string());
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("failed to bind {listen}"))?;

    let (tray, mut tray_commands) = if enable_tray {
        match ReceiverTrayHandle::spawn(
            format!("Listen: {listen}"),
            backend.label().to_string(),
            initial_pairing_armed,
        )
        .await
        {
            Ok((tray, commands)) => {
                tray.listening().await;
                (Some(tray), Some(commands))
            }
            Err(err) => {
                tracing::warn!(%err, "failed to start tray status item");
                append_portable_log(
                    &log_path,
                    format!("failed to start tray status item: {err:#}"),
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    tracing::info!(
        listen,
        fingerprint = %identity.fingerprint(),
        pairing_armed = initial_pairing_armed,
        "receiver listening"
    );
    append_portable_log(
        &log_path,
        format!(
            "receiver listening on {listen}; fingerprint={}; pairing_armed={initial_pairing_armed}",
            identity.fingerprint(),
        ),
    );

    let mut connection_enabled = true;
    let mut pairing_armed = initial_pairing_armed;
    loop {
        let (stream, addr) = tokio::select! {
            command = recv_tray_command(&mut tray_commands) => {
                match command {
                    TrayCommandEvent::Command(TrayCommand::OpenSettings) => {
                        open_receiver_settings(&config_path, &log_path);
                    }
                    TrayCommandEvent::Command(TrayCommand::ArmPairing) => {
                        pairing_armed = true;
                        if let Some(tray) = &tray {
                            tray.pairing_armed(true).await;
                            tray.listening().await;
                        }
                        tracing::info!("pairing armed from tray");
                        append_portable_log(
                            &log_path,
                            "pairing armed; waiting for confirmation on both computers",
                        );
                    }
                    TrayCommandEvent::Command(TrayCommand::Quit) => {
                        tracing::info!("quit requested from tray");
                        append_portable_log(&log_path, "quit requested from tray");
                        break;
                    }
                    TrayCommandEvent::Command(TrayCommand::Disconnect) => {
                        connection_enabled = false;
                        if let Some(tray) = &tray {
                            tray.disconnected_by_user().await;
                        }
                    }
                    TrayCommandEvent::Command(TrayCommand::Reconnect) => {
                        connection_enabled = true;
                        if let Some(tray) = &tray {
                            tray.listening().await;
                        }
                        tracing::info!("reconnect requested from tray");
                        append_portable_log(&log_path, "reconnect requested from tray");
                    }
                    TrayCommandEvent::Command(TrayCommand::SetAudio(_)) => {
                        tracing::info!("audio route ignored while no peer is connected");
                    }
                    TrayCommandEvent::Command(TrayCommand::ToggleInputForwarding) => {
                        tracing::info!(
                            "input forwarding toggle ignored while no controller is connected"
                        );
                    }
                    TrayCommandEvent::Command(TrayCommand::SetController(_)) => {
                        tracing::info!("role selection ignored while no peer is connected");
                    }
                    TrayCommandEvent::Closed => {
                        tracing::warn!("tray command channel closed; continuing without tray commands");
                        append_portable_log(
                            &log_path,
                            "tray command channel closed; continuing without tray commands",
                        );
                    }
                }
                continue;
            }
            incoming = listener.accept(), if connection_enabled => incoming?,
        };
        if let Err(err) = configure_controller_socket(&stream) {
            tracing::warn!(%err, %addr, "failed to configure controller socket timeout");
            append_portable_log(
                &log_path,
                format!("failed to configure controller socket timeout for {addr}: {err:#}"),
            );
        }
        tracing::info!(%addr, "controller connected");
        append_portable_log(&log_path, format!("controller connected: {addr}"));

        let (mut session, peer_fingerprint) = match time::timeout(
            Duration::from_secs(15),
            accept_noise_session(stream, &identity),
        )
        .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(err)) => {
                if let Some(tray) = &tray {
                    tray.error(format!("Noise handshake failed: {err}")).await;
                }
                tracing::warn!(%err, "Noise handshake failed");
                append_portable_log(&log_path, format!("Noise handshake failed: {err:#}"));
                continue;
            }
            Err(_) => {
                let message = "Noise handshake timed out";
                if let Some(tray) = &tray {
                    tray.error(message.to_string()).await;
                }
                tracing::warn!(message);
                append_portable_log(&log_path, message);
                continue;
            }
        };

        let hello =
            match time::timeout(Duration::from_secs(15), read_secure_frame(&mut session)).await {
                Ok(Ok(Frame::Hello(hello))) => hello,
                Ok(Ok(other)) => {
                    tracing::warn!(?other, "first frame was not Hello");
                    continue;
                }
                Ok(Err(err)) => {
                    if let Some(tray) = &tray {
                        tray.error(format!("failed to read Hello: {err}")).await;
                    }
                    tracing::warn!(%err, "failed to read Hello");
                    append_portable_log(&log_path, format!("failed to read Hello: {err:#}"));
                    continue;
                }
                Err(_) => {
                    let message = "timed out waiting for controller hello";
                    if let Some(tray) = &tray {
                        tray.error(message.to_string()).await;
                    }
                    tracing::warn!(message);
                    append_portable_log(&log_path, message);
                    continue;
                }
            };

        if let Err(error) = validate_controller_hello(&hello, &peer_fingerprint) {
            reject_pairing(&mut session, "invalid_hello", &error.to_string()).await;
            if let Some(tray) = &tray {
                tray.error(error.to_string()).await;
            }
            append_portable_log(&log_path, format!("rejected controller hello: {error:#}"));
            continue;
        }

        let pin_status = pins.status(&hello.device_name, &peer_fingerprint);
        let controller_trusted = pin_status.is_trusted();
        let controller_supports_pairing_confirmation = hello
            .extensions
            .iter()
            .any(|extension| extension == PAIRING_CONFIRMATION_EXTENSION);

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
                    AUDIO_ROUTE_EXTENSION.to_string(),
                ],
                node_capabilities: vec![
                    NodeCapability::InputCaptureV1,
                    NodeCapability::InputInjectV1,
                    NodeCapability::RoleSwitchV1,
                    NodeCapability::ScreenInfoBothSidesV1,
                    NodeCapability::AudioCaptureV1,
                    NodeCapability::AudioPlaybackV1,
                ],
            }),
        )
        .await?;

        if controller_supports_pairing_confirmation {
            write_secure_frame(
                &mut session,
                &Frame::Pairing(PairingEvent::Status {
                    trusted: controller_trusted,
                    armed: pairing_armed,
                }),
            )
            .await?;
            let (peer_trusted, peer_armed) = match read_pairing_status(&mut session).await {
                Ok(status) => status,
                Err(error) => {
                    if let Some(tray) = &tray {
                        tray.error(error.to_string()).await;
                    }
                    append_portable_log(
                        &log_path,
                        format!("failed pairing status exchange: {error:#}"),
                    );
                    continue;
                }
            };
            if !controller_trusted || !peer_trusted {
                if !pairing_armed || !peer_armed {
                    let message =
                        "pairing must be enabled from the tray on both computers before connecting";
                    reject_pairing(&mut session, "pairing_not_armed", message).await;
                    if let Some(tray) = &tray {
                        tray.error(message.to_string()).await;
                    }
                    append_portable_log(&log_path, message);
                    continue;
                }

                let confirmation = PairingConfirmationInput {
                    peer_name: hello.device_name.clone(),
                    peer_addr: Some(addr.to_string()),
                    local_fingerprint: identity.fingerprint(),
                    peer_fingerprint: peer_fingerprint.clone(),
                    verification_code: pairing_code(&identity.fingerprint(), &peer_fingerprint),
                    previous_peer_fingerprint: match &pin_status {
                        PinStatus::Changed { expected } => Some(expected.clone()),
                        PinStatus::Trusted | PinStatus::Unknown => None,
                    },
                };
                let accepted = match tokio::task::spawn_blocking(move || {
                    edge_ui::run_pairing_confirmation(confirmation)
                })
                .await
                {
                    Ok(Ok(accepted)) => accepted,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "pairing confirmation window failed");
                        false
                    }
                    Err(error) => {
                        tracing::warn!(%error, "pairing confirmation task failed");
                        false
                    }
                };
                write_secure_frame(
                    &mut session,
                    &Frame::Pairing(PairingEvent::Decision { accepted }),
                )
                .await?;
                let peer_accepted = if accepted {
                    read_pairing_decision(&mut session).await.unwrap_or(false)
                } else {
                    false
                };
                pairing_armed = false;
                if let Some(tray) = &tray {
                    tray.pairing_armed(false).await;
                }
                if !accepted || !peer_accepted {
                    let message = if accepted {
                        "pairing was declined on the peer"
                    } else {
                        "pairing was cancelled on this computer"
                    };
                    if let Some(tray) = &tray {
                        tray.error(message.to_string()).await;
                    }
                    append_portable_log(&log_path, message);
                    continue;
                }

                pins.pin(hello.device_name.clone(), peer_fingerprint.clone());
                pins.save(state_dir.join("pins.toml")).await?;
                tracing::info!(fingerprint = %peer_fingerprint, "paired peer after two-sided confirmation");
                append_portable_log(
                    &log_path,
                    format!(
                        "paired peer {} ({peer_fingerprint}) after confirmation on both computers",
                        hello.device_name
                    ),
                );
            }
        } else if !controller_trusted {
            let message = "peer does not support two-sided pairing confirmation; update it first";
            reject_pairing(&mut session, "pairing_update_required", message).await;
            if let Some(tray) = &tray {
                tray.error(message.to_string()).await;
            }
            append_portable_log(&log_path, message);
            continue;
        }

        if pairing_armed {
            pairing_armed = false;
            if let Some(tray) = &tray {
                tray.pairing_armed(false).await;
            }
        }

        if let Some(tray) = &tray {
            tray.connected(format!("{} ({peer_fingerprint})", hello.device_name))
                .await;
        }
        let controller_supports_audio_playback = hello
            .node_capabilities
            .contains(&NodeCapability::AudioPlaybackV1);
        let controller_supports_audio_capture = hello
            .node_capabilities
            .contains(&NodeCapability::AudioCaptureV1);
        let controller_supports_audio_route = hello
            .extensions
            .iter()
            .any(|extension| extension == AUDIO_ROUTE_EXTENSION);
        let controller_supports_input_capture = hello
            .node_capabilities
            .contains(&NodeCapability::InputCaptureV1);
        let controller_supports_input_injection = hello
            .node_capabilities
            .contains(&NodeCapability::InputInjectV1);
        let controller_supports_role_switch = hello
            .node_capabilities
            .contains(&NodeCapability::RoleSwitchV1);
        let controller_supports_input_toggle = hello
            .extensions
            .iter()
            .any(|extension| extension == INPUT_TOGGLE_EXTENSION);
        let controller_supports_images = hello
            .extensions
            .iter()
            .any(|extension| extension == CLIPBOARD_IMAGE_EXTENSION);
        if let Some(tray) = &tray {
            tray.audio_route(
                tray::AudioChoice::Off,
                controller_supports_audio_playback,
                controller_supports_audio_capture && controller_supports_audio_route,
            )
            .await;
        }

        let requested_monitor = config.input.inject.output.as_str();
        let screen_info = match hyprland_screen_info(requested_monitor).await {
            Ok(info) => {
                write_secure_frame(&mut session, &Frame::ScreenInfo(info.clone())).await?;
                Some(info)
            }
            Err(err) => {
                tracing::warn!(%err, "failed to query Hyprland monitor geometry");
                None
            }
        };

        let audio_bind = if addr.ip().is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let audio_socket = Arc::new(
            UdpSocket::bind(audio_bind)
                .await
                .context("failed to bind Linux audio UDP socket")?,
        );

        match handle_controller(
            session,
            &config,
            &config_path,
            &backend,
            tray.as_ref(),
            &mut tray_commands,
            &log_path,
            screen_info,
            audio_socket,
            addr.ip(),
            controller_supports_audio_playback,
            controller_supports_audio_capture,
            controller_supports_audio_route,
            controller_supports_input_toggle,
            controller_supports_images,
            controller_supports_input_capture,
            controller_supports_input_injection,
            controller_supports_role_switch,
            &peer_fingerprint,
            &identity.fingerprint(),
            &hello.device_name,
        )
        .await
        {
            Ok(ControllerSessionExit::QuitRequested) => {
                tracing::info!("quit requested from tray");
                append_portable_log(&log_path, "quit requested from tray");
                break;
            }
            Ok(ControllerSessionExit::DisconnectRequested) => {
                connection_enabled = false;
                if let Some(tray) = &tray {
                    tray.disconnected_by_user().await;
                }
                tracing::info!("disconnect requested from tray");
                append_portable_log(&log_path, "disconnect requested from tray");
            }
            Err(err) => {
                if let Some(tray) = &tray {
                    tray.disconnected(Some(err.to_string())).await;
                }
                tracing::warn!(%err, "controller session ended");
                append_portable_log(&log_path, format!("controller session ended: {err:#}"));
            }
        }
        backend.all_keys_up().await.ok();
    }

    backend.all_keys_up().await.ok();
    if let Some(tray) = &tray {
        tray.shutdown().await;
    }
    append_portable_log(&log_path, "receiver shutdown complete");
    Ok(())
}

fn configure_controller_socket(stream: &TcpStream) -> Result<()> {
    #[cfg(target_os = "linux")]
    SockRef::from(stream)
        .set_tcp_user_timeout(Some(CONTROLLER_STALL_TIMEOUT))
        .context("failed to set TCP_USER_TIMEOUT")?;
    let _ = stream;
    Ok(())
}

fn install_receiver_panic_log(log_path: PathBuf) {
    std::panic::set_hook(Box::new(move |panic_info| {
        append_portable_log(&log_path, format!("receiver panic: {panic_info}"));
    }));
}

fn append_portable_log(path: &Path, message: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{:?} {}", SystemTime::now(), message.as_ref());
    }
}

fn open_receiver_settings(config_path: &Path, log_path: &Path) {
    if SETTINGS_PROCESS_OPEN.swap(true, Ordering::AcqRel) {
        return;
    }

    append_portable_log(log_path, "opening settings window");
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(err) => {
            SETTINGS_PROCESS_OPEN.store(false, Ordering::Release);
            tracing::warn!(%err, "failed to locate receiver executable for settings window");
            append_portable_log(
                log_path,
                format!("failed to locate receiver executable for settings window: {err}"),
            );
            return;
        }
    };

    match Command::new(executable)
        .arg("--settings")
        .arg("--config")
        .arg(config_path)
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
                SETTINGS_PROCESS_OPEN.store(false, Ordering::Release);
            });
        }
        Err(err) => {
            SETTINGS_PROCESS_OPEN.store(false, Ordering::Release);
            tracing::warn!(%err, "failed to start settings process");
            append_portable_log(log_path, format!("failed to start settings process: {err}"));
        }
    }
}

enum ControllerSessionExit {
    QuitRequested,
    DisconnectRequested,
}

struct AbortOnDropTask(JoinHandle<()>);

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct AudioStartResult {
    generation: u64,
    result: std::result::Result<(std::net::SocketAddr, edge_linux_audio::LinuxAudioSender), String>,
}

enum ClipboardWatchEvent {
    Changed,
    Closed,
}

async fn recv_clipboard_change(
    watcher: &mut Option<ClipboardChangeWatcher>,
) -> ClipboardWatchEvent {
    match watcher {
        Some(watcher) => match watcher.recv().await {
            Some(()) => ClipboardWatchEvent::Changed,
            None => ClipboardWatchEvent::Closed,
        },
        None => future::pending().await,
    }
}

fn validate_controller_hello(hello: &Hello, noise_fingerprint: &str) -> Result<()> {
    if hello.protocol_version != PROTOCOL_VERSION {
        anyhow::bail!(
            "Upgrade the other computer: peer protocol version {} is incompatible with {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        );
    }
    if hello.public_key_fingerprint != noise_fingerprint {
        anyhow::bail!("peer hello fingerprint does not match its encrypted identity");
    }
    Ok(())
}

async fn reject_pairing(session: &mut NoiseSession<TcpStream>, code: &str, message: &str) {
    write_secure_frame(
        session,
        &Frame::Error(RemoteError {
            code: code.to_string(),
            message: message.to_string(),
        }),
    )
    .await
    .ok();
}

async fn read_pairing_status(session: &mut NoiseSession<TcpStream>) -> Result<(bool, bool)> {
    match time::timeout(Duration::from_secs(15), read_secure_frame(session)).await {
        Ok(Ok(Frame::Pairing(PairingEvent::Status { trusted, armed }))) => Ok((trusted, armed)),
        Ok(Ok(Frame::Error(error))) => {
            anyhow::bail!("peer error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected peer pairing status, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read peer pairing status"),
        Err(_) => anyhow::bail!("timed out waiting for peer pairing status"),
    }
}

async fn read_pairing_decision(session: &mut NoiseSession<TcpStream>) -> Result<bool> {
    match time::timeout(Duration::from_secs(120), read_secure_frame(session)).await {
        Ok(Ok(Frame::Pairing(PairingEvent::Decision { accepted }))) => Ok(accepted),
        Ok(Ok(Frame::Error(error))) => {
            anyhow::bail!("peer error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected peer pairing decision, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read peer pairing decision"),
        Err(_) => anyhow::bail!("timed out waiting for pairing confirmation on the peer"),
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
async fn handle_controller(
    session: NoiseSession<TcpStream>,
    config: &AppConfig,
    config_path: &Path,
    backend: &ReceiverBackend,
    tray: Option<&ReceiverTrayHandle>,
    tray_commands: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
    log_path: &Path,
    screen_info: Option<ScreenInfo>,
    audio_socket: Arc<UdpSocket>,
    controller_ip: std::net::IpAddr,
    controller_supports_audio_playback: bool,
    controller_supports_audio_capture: bool,
    controller_supports_audio_route: bool,
    controller_supports_input_toggle: bool,
    controller_supports_images: bool,
    controller_supports_input_capture: bool,
    controller_supports_input_injection: bool,
    controller_supports_role_switch: bool,
    controller_fingerprint: &str,
    local_fingerprint: &str,
    controller_name: &str,
) -> Result<ControllerSessionExit> {
    use edge_linux_input::{CaptureEvent, PortalCaptureBackend};

    let mut heartbeat_sequence = 0_u64;
    let liveness_config = LivenessConfig::default();
    let heartbeat = time::sleep(liveness_config.heartbeat_interval(false));
    tokio::pin!(heartbeat);
    let mut connection_watchdog = time::interval(Duration::from_millis(250));
    connection_watchdog.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut controller_liveness =
        LivenessTracker::new(liveness_config, tokio::time::Instant::now());
    let mut input_epoch = InputEpochGate::default();
    let mut status_log = time::interval(STATUS_LOG_INTERVAL);
    let mut stats = ReceiverInputStats::default();
    let (reader, mut writer) = SecureFrameSession::new(session).split();
    let mut frame_rx = spawn_controller_reader(reader);
    let mut return_watcher = RemoteReturnWatcher::new(screen_info.clone());
    let mut clipboard_sync =
        ReceiverClipboardState::new(config, controller_supports_images).await?;
    let mut clipboard_send = time::interval(Duration::from_millis(2));
    clipboard_send.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut clipboard_watcher = config
        .clipboard
        .enabled
        .then(spawn_clipboard_change_watcher);
    let mut audio_sender: Option<edge_linux_audio::LinuxAudioSender> = None;
    let mut audio_receiver: Option<edge_linux_audio::LinuxAudioReceiver> = None;
    let (audio_start_tx, mut audio_start_rx) = mpsc::unbounded_channel::<AudioStartResult>();
    let mut audio_start_task: Option<AbortOnDropTask> = None;
    let mut audio_start_generation = 0_u64;
    let mut audio_source: Option<String> = None;
    let mut input_forwarding_enabled = true;
    let mut session_paused = false;
    let mut role_epoch = INITIAL_ROLE_EPOCH;
    let role_switch_available = controller_supports_role_switch
        && controller_supports_input_capture
        && controller_supports_input_injection;
    let mut current_role_state = edge_protocol::RoleState {
        controller_fingerprint: Some(controller_fingerprint.to_string()),
        role_epoch,
        transition: edge_protocol::RoleTransitionState::Stable,
        listener_position: peer_position_to_edge(config.layout.listener_position),
        paused: false,
        failure_detail: None,
    };
    let mut local_is_controller = false;
    let mut capture_input_active = false;
    let mut capture: Option<PortalCaptureBackend> = None;
    let mut prepared_role: Option<edge_protocol::RoleState> = None;
    let mut role_request_deadline: Option<tokio::time::Instant> = None;
    let role_store = RoleStore::new(default_state_dir().join("role.toml"));
    if let Some(tray) = tray {
        tray.input_forwarding(true).await;
        tray.audio_route(
            tray::AudioChoice::Off,
            controller_supports_audio_playback,
            controller_supports_audio_capture && controller_supports_audio_route,
        )
        .await;
    }

    loop {
        if !session_paused && writer.bulk_is_due() && clipboard_sync.outgoing.is_some() {
            match clipboard_sync.send_next_image_frame(&mut writer).await {
                Ok(true) => {
                    tracing::info!("completed Linux clipboard image transfer to controller");
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "failed to send Linux clipboard image chunk");
                    clipboard_sync.outgoing = None;
                }
            }
            continue;
        }

        tokio::select! {
            biased;
            _ = connection_watchdog.tick() => {
                if role_request_deadline.is_some_and(|deadline| {
                    tokio::time::Instant::now() >= deadline
                }) {
                    role_request_deadline = None;
                    prepared_role = None;
                    if local_is_controller
                        && input_forwarding_enabled
                        && !session_paused
                        && let Some(capture) = &capture
                    {
                        capture.arm().await.ok();
                    }
                    if let Some(tray) = tray {
                        tray.role_assignment(
                            config.device_name.clone(),
                            controller_name.to_string(),
                            local_is_controller,
                            role_switch_available && !session_paused,
                        )
                        .await;
                    }
                    tracing::warn!("role request timed out; retained committed assignment");
                }
                let input_active = if local_is_controller {
                    capture_input_active
                } else {
                    input_epoch.accepts_input()
                };
                match controller_liveness.poll(tokio::time::Instant::now(), input_active) {
                    Some(LivenessEvent::SoftInputTimeout) => {
                        capture_input_active = false;
                        input_epoch.suspend();
                        backend.all_keys_up().await.ok();
                        if let Some(capture) = &capture {
                            capture.release(None).await.ok();
                        }
                        tracing::warn!("peer was silent for one second; released injected input and suspended its input epoch");
                        append_portable_log(log_path, "peer liveness soft timeout; released injected input and suspended stale frames");
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::control(role_epoch, ControlEvent::ReleaseToLocal {
                                reason: ReleaseReason::HeartbeatTimeout,
                            }),
                        )
                        .await
                        .ok();
                    }
                    Some(LivenessEvent::HardSessionTimeout) => {
                        anyhow::bail!(
                            "peer stopped responding for {:?}; closing session",
                            controller_liveness.elapsed(tokio::time::Instant::now())
                        );
                    }
                    None => {}
                }
            }
            event = recv_portal_capture_event(&mut capture) => {
                match event {
                    Some(CaptureEvent::Activated { edge, normalized_position, .. })
                        if local_is_controller && input_forwarding_enabled && !session_paused =>
                    {
                        capture_input_active = true;
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::control(role_epoch, ControlEvent::EnterRemote {
                                edge,
                                normalized_position,
                            }),
                        )
                        .await?;
                    }
                    Some(CaptureEvent::Input(event))
                        if local_is_controller && input_forwarding_enabled && !session_paused =>
                    {
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::input(role_epoch, event),
                        )
                        .await?;
                        if let Some(tray) = tray {
                            tray.input_event().await;
                        }
                    }
                    Some(CaptureEvent::Deactivated | CaptureEvent::EmergencyReleased) => {
                        capture_input_active = false;
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::input(role_epoch, InputEvent::AllKeysUp),
                        )
                        .await
                        .ok();
                    }
                    Some(CaptureEvent::LayoutChanged { .. }) => {
                        capture_input_active = false;
                        if let Some(prepared) = prepared_role.take() {
                            role_request_deadline = None;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::Ready {
                                    role_epoch: prepared.role_epoch,
                                    capture_ready: false,
                                    inject_ready: true,
                                    failure_detail: Some(
                                        "monitor layout changed during capture preflight".to_string(),
                                    ),
                                }),
                            )
                            .await
                            .ok();
                        } else if local_is_controller
                            && input_forwarding_enabled
                            && !session_paused
                            && let Some(capture) = &capture
                        {
                            capture.arm().await?;
                        }
                    }
                    Some(CaptureEvent::BackendFailed(error)) => {
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::input(role_epoch, InputEvent::AllKeysUp),
                        )
                        .await
                        .ok();
                        anyhow::bail!("Linux capture backend failed: {error}");
                    }
                    None if capture.is_some() && local_is_controller => {
                        anyhow::bail!("Linux capture backend stopped unexpectedly");
                    }
                    _ => {}
                }
            }
            _ = &mut heartbeat => {
                if let Some(transfer_id) = clipboard_sync.incoming.expire_transfer_id() {
                    tracing::warn!(transfer_id, "expired incomplete controller clipboard image");
                    write_secure_frame_writer(
                        &mut writer,
                        &Frame::Clipboard(ClipboardEvent::ImageCancel {
                            transfer_id,
                            reason: ClipboardCancelReason::TimedOut,
                        }),
                    )
                    .await?;
                }
                heartbeat_sequence += 1;
                write_secure_frame_writer(&mut writer, &Frame::Heartbeat(Heartbeat { sequence: heartbeat_sequence })).await?;
                stats.heartbeats = stats.heartbeats.saturating_add(1);
                let input_active = if local_is_controller {
                    capture_input_active
                } else {
                    input_epoch.accepts_input()
                };
                heartbeat.as_mut().reset(
                    tokio::time::Instant::now()
                        + liveness_config.heartbeat_interval(
                            input_active || prepared_role.is_some(),
                        ),
                );
            }
            _ = status_log.tick() => {
                stats.log(log_path, "receiver");
                if audio_sender.as_ref().is_some_and(|sender| sender.is_finished()) {
                    if let Some(sender) = audio_sender.take() {
                        sender.stop().await.ok();
                    }
                    tracing::warn!("Linux audio capture task stopped unexpectedly");
                    append_portable_log(log_path, "Linux audio capture task stopped unexpectedly; routing restored");
                    write_secure_frame_writer(
                        &mut writer,
                        &Frame::Audio(AudioControl::State {
                            state: AudioStreamState::Error,
                            detail: Some("Linux audio capture stopped unexpectedly".to_string()),
                        }),
                    )
                    .await
                    .ok();
                }
                if audio_receiver.as_ref().is_some_and(|receiver| receiver.is_finished())
                    && let Some(receiver) = audio_receiver.take()
                {
                    let failure = receiver.failure_reason().await;
                    tracing::warn!(%failure, "Linux audio playback stopped unexpectedly");
                    write_secure_frame_writer(
                        &mut writer,
                        &Frame::Audio(AudioControl::State {
                            state: AudioStreamState::Error,
                            detail: Some(failure),
                        }),
                    ).await.ok();
                }
            }
            Some(started) = audio_start_rx.recv(), if audio_start_task.is_some() => {
                if started.generation != audio_start_generation {
                    if let Ok((_, sender)) = started.result {
                        sender.stop().await.ok();
                    }
                    continue;
                }
                audio_start_task = None;
                match started.result {
                    Ok((destination, sender)) => {
                        audio_sender = Some(sender);
                        append_portable_log(
                            log_path,
                            format!("authenticated Windows audio UDP endpoint: {destination}"),
                        );
                        append_portable_log(log_path, "Linux audio capture is streaming");
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Streaming,
                                detail: None,
                            }),
                        )
                        .await?;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to start Linux audio transport");
                        append_portable_log(
                            log_path,
                            format!("failed to start Linux audio transport: {error}"),
                        );
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Error,
                                detail: Some(error),
                            }),
                        )
                        .await?;
                    }
                }
            }
            command = recv_tray_command(tray_commands) => {
                match command {
                    TrayCommandEvent::Command(TrayCommand::OpenSettings) => {
                        open_receiver_settings(config_path, log_path);
                    }
                    TrayCommandEvent::Command(TrayCommand::ArmPairing) => {
                        tracing::debug!("ignored pairing request while connected");
                    }
                    TrayCommandEvent::Command(TrayCommand::Quit) => {
                        drop(audio_start_task.take());
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        drop(audio_receiver.take());
                        return Ok(ControllerSessionExit::QuitRequested);
                    }
                    TrayCommandEvent::Command(TrayCommand::Disconnect) => {
                        if controller_supports_role_switch {
                            session_paused = true;
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            audio_receiver = None;
                            backend.all_keys_up().await.ok();
                            if let Some(capture) = &capture {
                                capture.release(None).await.ok();
                                capture.disarm().await.ok();
                            }
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::SetPaused { paused: true }),
                            )
                            .await?;
                            if let Some(tray) = tray {
                                tray.session_paused(true).await;
                                tray.role_assignment(
                                    config.device_name.clone(),
                                    controller_name.to_string(),
                                    local_is_controller,
                                    false,
                                )
                                .await;
                            }
                            continue;
                        }
                        drop(audio_start_task.take());
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        drop(audio_receiver.take());
                        backend.all_keys_up().await.ok();
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::control(role_epoch, ControlEvent::ReleaseToLocal {
                                reason: ReleaseReason::UserRequest,
                            }),
                        )
                        .await
                        .ok();
                        return Ok(ControllerSessionExit::DisconnectRequested);
                    }
                    TrayCommandEvent::Command(TrayCommand::Reconnect) => {
                        if controller_supports_role_switch {
                            session_paused = false;
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::SetPaused { paused: false }),
                            )
                            .await?;
                            if local_is_controller
                                && input_forwarding_enabled
                                && let Some(capture) = &capture
                            {
                                capture.arm().await?;
                            }
                            if let Some(tray) = tray {
                                tray.session_paused(false).await;
                                tray.role_assignment(
                                    config.device_name.clone(),
                                    controller_name.to_string(),
                                    local_is_controller,
                                    role_switch_available,
                                )
                                .await;
                            }
                        }
                    }
                    TrayCommandEvent::Command(TrayCommand::SetController(choice)) => {
                        let requested = match choice {
                            ControllerChoice::Local => local_fingerprint,
                            ControllerChoice::Peer => controller_fingerprint,
                        };
                        if session_paused || !role_switch_available {
                            tracing::warn!("role switch is unavailable for this peer");
                        } else if prepared_role.is_none()
                            && role_request_deadline.is_none()
                            && current_role_state.controller_fingerprint.as_deref()
                                != Some(requested)
                        {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::Request {
                                    controller_fingerprint: requested.to_string(),
                                }),
                            )
                            .await?;
                            role_request_deadline = Some(
                                tokio::time::Instant::now() + Duration::from_secs(5),
                            );
                            if let Some(tray) = tray {
                                tray.role_switching(true).await;
                            }
                        }
                    }
                    TrayCommandEvent::Command(TrayCommand::ToggleInputForwarding) => {
                        if prepared_role.is_some() {
                            continue;
                        }
                        input_forwarding_enabled = !input_forwarding_enabled;
                        if !input_forwarding_enabled {
                            backend.all_keys_up().await.ok();
                            return_watcher.record_control(
                                &ControlEvent::SetInputForwarding { enabled: false },
                            );
                            if let Some(capture) = &capture {
                                capture.disarm().await.ok();
                            }
                        } else if local_is_controller && let Some(capture) = &capture {
                            capture.arm().await?;
                        }
                        if let Some(tray) = tray {
                            tray.input_forwarding(input_forwarding_enabled).await;
                        }
                        append_portable_log(
                            log_path,
                            format!(
                                "input forwarding toggled to {input_forwarding_enabled} from receiver"
                            ),
                        );
                        if controller_supports_input_toggle {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::control(role_epoch, ControlEvent::SetInputForwarding {
                                    enabled: input_forwarding_enabled,
                                }),
                            )
                            .await?;
                        } else if !input_forwarding_enabled {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::control(role_epoch, ControlEvent::ReleaseToLocal {
                                    reason: ReleaseReason::BackendFailure,
                                }),
                            )
                            .await?;
                        }
                    }
                    TrayCommandEvent::Command(TrayCommand::SetAudio(choice)) => {
                        let source_fingerprint = match choice {
                            tray::AudioChoice::Off => None,
                            tray::AudioChoice::Local if controller_supports_audio_playback => {
                                Some(local_fingerprint.to_string())
                            }
                            tray::AudioChoice::Peer if controller_supports_audio_capture && controller_supports_audio_route => {
                                Some(controller_fingerprint.to_string())
                            }
                            _ => {
                                tracing::warn!(?choice, "requested audio direction is unavailable");
                                continue;
                            }
                        };
                        let control = if controller_supports_audio_route {
                            AudioControl::RequestRoute { source_fingerprint }
                        } else {
                            AudioControl::SetEnabled {
                                enabled: source_fingerprint.as_deref() == Some(local_fingerprint),
                            }
                        };
                        write_secure_frame_writer(&mut writer, &Frame::Audio(control)).await?;
                    }
                    TrayCommandEvent::Closed => {
                        tracing::warn!("tray command channel closed; continuing session without tray commands");
                        append_portable_log(
                            log_path,
                            "tray command channel closed; continuing session without tray commands",
                        );
                    }
                }
            }
            event = recv_clipboard_change(&mut clipboard_watcher) => {
                match event {
                    ClipboardWatchEvent::Changed => {
                        if session_paused {
                            continue;
                        }
                        match clipboard_sync.send_changed_offer(config, &mut writer).await {
                            Ok(true) => {
                                stats.clipboard = stats.clipboard.saturating_add(1);
                                tracing::info!("sent changed Linux clipboard to controller");
                                if let Some(tray) = tray {
                                    tray.clipboard_event().await;
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "failed to synchronize changed Linux clipboard");
                                append_portable_log(log_path, format!("failed to synchronize changed Linux clipboard: {error}"));
                            }
                        }
                    }
                    ClipboardWatchEvent::Closed => {
                        tracing::warn!("Wayland clipboard watcher stopped; restarting");
                        append_portable_log(log_path, "Wayland clipboard watcher stopped; restarting");
                        clipboard_watcher = Some(spawn_clipboard_change_watcher());
                    }
                }
            }
            frame = frame_rx.recv() => {
                let frame = frame.context("controller frame reader ended")??;
                controller_liveness.observe_authenticated_frame(tokio::time::Instant::now());
                writer.record_received(&frame);
                match frame {
                    Frame::Input(input) => {
                        if input.role_epoch != role_epoch {
                            tracing::debug!(
                                role_epoch = input.role_epoch,
                                "ignored input from a stale role epoch"
                            );
                            continue;
                        }
                        if session_paused || local_is_controller {
                            tracing::trace!("ignored input while this node is the controller");
                            continue;
                        }
                        let event = input.event;
                        if event == InputEvent::AllKeysUp {
                            stats.all_keys_up = stats.all_keys_up.saturating_add(1);
                            input_epoch.suspend();
                            backend.all_keys_up().await?;
                            if let Some(tray) = tray {
                                tray.input_event().await;
                            }
                            continue;
                        }
                        if !input_forwarding_enabled {
                            tracing::trace!(?event, "ignored input while forwarding is disabled");
                            continue;
                        }
                        if !input_epoch.accepts_input() {
                            tracing::trace!(?event, "ignored stale input from a suspended input epoch");
                            continue;
                        }
                        stats.record_input(&event);
                        let is_motion = matches!(event, InputEvent::PointerMotion { .. });
                        backend.inject(event).await?;
                        if is_motion {
                            match return_watcher.release_if_at_edge().await {
                                Ok(Some(control)) => {
                                    stats.return_releases = stats.return_releases.saturating_add(1);
                                    tracing::info!(?control, "real cursor reached return edge");
                                    append_portable_log(
                                        log_path,
                                        format!("real cursor reached return edge: {control:?}"),
                                    );
                                    input_epoch.suspend();
                                    backend.all_keys_up().await.ok();
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::control(role_epoch, control),
                                    )
                                    .await?;
                                }
                                Ok(None) => {}
                                Err(err) => tracing::warn!(%err, "failed to check Hyprland cursor position"),
                            }
                        }
                        if let Some(tray) = tray {
                            tray.input_event().await;
                        }
                    }
                    Frame::Clipboard(event) => {
                        if session_paused {
                            continue;
                        }
                        match clipboard_sync.handle_event(config, &mut writer, event).await {
                            Ok(true) => {
                                stats.clipboard = stats.clipboard.saturating_add(1);
                                if let Some(tray) = tray {
                                    tray.clipboard_event().await;
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "failed to synchronize controller clipboard");
                                append_portable_log(log_path, format!("failed to synchronize controller clipboard: {error}"));
                            }
                        }
                    }
                    Frame::Heartbeat(_) => {}
                    Frame::Control(control) => {
                        if control.role_epoch != role_epoch {
                            tracing::debug!(
                                role_epoch = control.role_epoch,
                                "ignored control from a stale role epoch"
                            );
                            continue;
                        }
                        let control = control.event;
                        if let ControlEvent::SetInputForwarding { enabled } = control {
                            if controller_supports_input_toggle {
                                input_forwarding_enabled = enabled;
                                if !enabled {
                                    capture_input_active = false;
                                    input_epoch.suspend();
                                    backend.all_keys_up().await.ok();
                                    if let Some(capture) = &capture {
                                        capture.disarm().await.ok();
                                    }
                                } else if local_is_controller
                                    && let Some(capture) = &capture
                                {
                                    capture.arm().await?;
                                }
                                return_watcher.record_control(
                                    &ControlEvent::SetInputForwarding { enabled },
                                );
                                if let Some(tray) = tray {
                                    tray.input_forwarding(enabled).await;
                                }
                                stats.control = stats.control.saturating_add(1);
                                append_portable_log(
                                    log_path,
                                    format!("controller set input forwarding to {enabled}"),
                                );
                            }
                            continue;
                        }
                        stats.control = stats.control.saturating_add(1);
                        tracing::info!(?control, "control event");
                        if local_is_controller {
                            match control {
                                ControlEvent::LeaveRemote {
                                    edge,
                                    normalized_position,
                                } => {
                                    capture_input_active = false;
                                    write_secure_frame_writer(
                                        &mut writer,
                                        &Frame::input(role_epoch, InputEvent::AllKeysUp),
                                    )
                                    .await?;
                                    if let Some(capture) = &capture {
                                        let cursor = linux_release_cursor(
                                            screen_info.as_ref(),
                                            edge,
                                            normalized_position,
                                        );
                                        capture.release(cursor).await?;
                                    }
                                }
                                ControlEvent::ReleaseToLocal { .. } => {
                                    capture_input_active = false;
                                    if let Some(capture) = &capture {
                                        capture.release(None).await?;
                                    }
                                }
                                ControlEvent::EnterRemote { .. }
                                | ControlEvent::SetInputForwarding { .. } => {}
                            }
                            continue;
                        }
                        input_epoch.observe_control(&control);
                        return_watcher.record_control(&control);
                        if should_release_legacy_controller(
                            input_forwarding_enabled,
                            controller_supports_input_toggle,
                            &control,
                        ) {
                            tracing::info!(
                                "releasing legacy controller that entered while input forwarding is disabled"
                            );
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::control(
                                    role_epoch,
                                    ControlEvent::ReleaseToLocal {
                                        reason: ReleaseReason::BackendFailure,
                                    },
                                ),
                            )
                            .await?;
                        }
                    }
                    Frame::Audio(AudioControl::Start {
                        udp_port,
                        session_id,
                        session_salt,
                        session_key,
                        codec,
                        frame_ms,
                        jitter_target_ms: _,
                    }) => {
                        if audio_source.as_deref() != Some(local_fingerprint) {
                            tracing::warn!("ignored audio start while this machine is not the source");
                            continue;
                        }
                        append_portable_log(log_path, "received peer audio start request");
                        if udp_port == 0
                            || codec != AudioCodec::PcmS16Stereo48Khz
                            || frame_ms != edge_audio::FRAME_MS
                        {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::State {
                                    state: AudioStreamState::Error,
                                    detail: Some("unsupported audio format".to_string()),
                                }),
                            )
                            .await?;
                            continue;
                        }
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        let generation = audio_start_generation;
                        let secrets = SessionSecrets {
                            session_id,
                            session_salt,
                            session_key,
                        };
                        let advertised_destination =
                            std::net::SocketAddr::new(controller_ip, udp_port);
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::WaitingForUdp,
                                detail: None,
                            }),
                        )
                        .await?;
                        append_portable_log(
                            log_path,
                            format!(
                                "establishing authenticated UDP audio path to {advertised_destination}"
                            ),
                        );
                        let redirect = config.audio.local_playback
                            == AudioLocalPlayback::Redirect;
                        let start_socket = audio_socket.clone();
                        let state_dir = default_state_dir();
                        let result_tx = audio_start_tx.clone();
                        audio_start_task = Some(AbortOnDropTask(tokio::spawn(async move {
                            let result = async {
                                let cipher = edge_audio::PacketCipher::new(&secrets);
                                let destination = edge_linux_audio::establish_peer(
                                    &start_socket,
                                    &cipher,
                                    advertised_destination,
                                    controller_ip,
                                    Duration::from_secs(3),
                                )
                                .await?;
                                let sender = edge_linux_audio::LinuxAudioSender::start(
                                    start_socket,
                                    destination,
                                    secrets,
                                    &state_dir,
                                    redirect,
                                )
                                .await?;
                                Ok::<_, anyhow::Error>((destination, sender))
                            }
                            .await
                            .map_err(|error| format!("{error:#}"));
                            let _ = result_tx.send(AudioStartResult { generation, result });
                        })));
                    }
                    Frame::Audio(AudioControl::SetRoute { source_fingerprint }) => {
                        let valid = source_fingerprint.as_deref().is_none_or(|source| {
                            source == local_fingerprint && controller_supports_audio_playback
                                || source == controller_fingerprint
                                    && controller_supports_audio_capture
                                    && controller_supports_audio_route
                        });
                        if !valid {
                            anyhow::bail!("connector committed an unsupported audio route");
                        }
                        audio_source = source_fingerprint;
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        audio_receiver = None;
                        let choice = match audio_source.as_deref() {
                            Some(source) if source == local_fingerprint => tray::AudioChoice::Local,
                            Some(source) if source == controller_fingerprint => tray::AudioChoice::Peer,
                            _ => tray::AudioChoice::Off,
                        };
                        if let Some(tray) = tray {
                            tray.audio_route(
                                choice,
                                controller_supports_audio_playback,
                                controller_supports_audio_capture && controller_supports_audio_route,
                            ).await;
                        }
                        if audio_source.as_deref() == Some(local_fingerprint) && !session_paused {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::Offer {
                                    udp_port: audio_socket.local_addr()?.port(),
                                    codecs: vec![AudioCodec::PcmS16Stereo48Khz],
                                }),
                            ).await?;
                        } else if audio_source.is_none() {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::State {
                                    state: AudioStreamState::Disabled,
                                    detail: None,
                                }),
                            ).await?;
                        }
                    }
                    Frame::Audio(AudioControl::Stop { reason }) => {
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        audio_receiver = None;
                        append_portable_log(log_path, format!("audio stopped by connector: {reason:?}"));
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Disabled,
                                detail: None,
                            }),
                        )
                        .await?;
                    }
                    Frame::Audio(AudioControl::Offer { udp_port, codecs }) => {
                        if audio_source.as_deref() != Some(controller_fingerprint)
                            || !codecs.contains(&AudioCodec::PcmS16Stereo48Khz)
                        {
                            continue;
                        }
                        let secrets = SessionSecrets::generate();
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::Start {
                                udp_port: audio_socket.local_addr()?.port(),
                                session_id: secrets.session_id,
                                session_salt: secrets.session_salt,
                                session_key: secrets.session_key,
                                codec: AudioCodec::PcmS16Stereo48Khz,
                                frame_ms: edge_audio::FRAME_MS,
                                jitter_target_ms: config.audio.jitter_target_ms as u16,
                            }),
                        ).await?;
                        match edge_linux_audio::LinuxAudioReceiver::start(
                            audio_socket.clone(),
                            std::net::SocketAddr::new(controller_ip, udp_port),
                            secrets,
                            config.audio.jitter_target_ms,
                        ).await {
                            Ok(receiver) => {
                                audio_receiver = Some(receiver);
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::Audio(AudioControl::State {
                                        state: AudioStreamState::Streaming,
                                        detail: None,
                                    }),
                                ).await?;
                            }
                            Err(error) => {
                                write_secure_frame_writer(
                                    &mut writer,
                                    &Frame::Audio(AudioControl::State {
                                        state: AudioStreamState::Error,
                                        detail: Some(error.to_string()),
                                    }),
                                ).await?;
                            }
                        }
                    }
                    Frame::Audio(AudioControl::RequestRoute { .. }) => {
                        tracing::warn!("ignored connector-only audio route request from connector");
                    }
                    Frame::Audio(AudioControl::SetEnabled { enabled }) => {
                        audio_source = enabled.then(|| local_fingerprint.to_string());
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        audio_receiver = None;
                        if let Some(tray) = tray {
                            tray.audio_route(
                                if enabled { tray::AudioChoice::Local } else { tray::AudioChoice::Off },
                                controller_supports_audio_playback,
                                false,
                            ).await;
                        }
                        if enabled && !session_paused {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::Offer {
                                    udp_port: audio_socket.local_addr()?.port(),
                                    codecs: vec![AudioCodec::PcmS16Stereo48Khz],
                                }),
                            ).await?;
                        } else {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Audio(AudioControl::State {
                                    state: AudioStreamState::Disabled,
                                    detail: None,
                                }),
                            ).await?;
                        }
                    }
                    Frame::Audio(AudioControl::State { .. }) => {}
                    Frame::Role(RoleEvent::SessionState(state)) => {
                        if state.controller_fingerprint.as_deref().is_some_and(|controller| {
                            controller != controller_fingerprint && controller != local_fingerprint
                        }) {
                            anyhow::bail!(
                                "connector role state names a controller outside the authenticated pair"
                            );
                        }
                        if state.role_epoch == 0
                            || !matches!(
                                state.transition,
                                edge_protocol::RoleTransitionState::Stable
                            )
                        {
                            anyhow::bail!("connector sent invalid zero role epoch");
                        }
                        role_epoch = state.role_epoch;
                        session_paused = state.paused;
                        local_is_controller = state.controller_fingerprint.as_deref()
                            == Some(local_fingerprint);
                        if local_is_controller {
                            backend.all_keys_up().await.ok();
                            if capture.is_none() {
                                capture = Some(
                                    PortalCaptureBackend::preflight(opposite_edge(
                                        state.listener_position,
                                    ))
                                    .await
                                    .context("listener capture preflight failed")?,
                                );
                            }
                            if input_forwarding_enabled
                                && !session_paused
                                && let Some(capture) = &capture
                            {
                                capture.arm().await?;
                            }
                        } else {
                            if let Some(capture) = &capture {
                                capture.release(None).await.ok();
                                capture.disarm().await.ok();
                            }
                        }
                        if state.paused || state.controller_fingerprint.is_none() {
                            input_epoch.suspend();
                            backend.all_keys_up().await.ok();
                        }
                        if let Some(controller) = &state.controller_fingerprint {
                            role_store
                                .save(&CommittedRole::new(controller))
                                .await
                                .context("failed to mirror connector role state")?;
                        }
                        current_role_state = state.clone();
                        capture_input_active = false;
                        prepared_role = None;
                        role_request_deadline = None;
                        if let Some(tray) = tray {
                            tray.input_forwarding(input_forwarding_enabled).await;
                            tray.session_paused(session_paused).await;
                            tray.role_assignment(
                                config.device_name.clone(),
                                controller_name.to_string(),
                                local_is_controller,
                                role_switch_available && !session_paused,
                            )
                            .await;
                        }
                        tracing::info!(
                            role_epoch,
                            listener_position = ?state.listener_position,
                            paused = state.paused,
                            "adopted connector-authoritative session state"
                        );
                    }
                    Frame::Role(RoleEvent::Prepare(prepare)) => {
                        if session_paused || !role_switch_available {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Role(RoleEvent::Ready {
                                    role_epoch: prepare.role_epoch,
                                    capture_ready: false,
                                    inject_ready: false,
                                    failure_detail: Some(
                                        "role switching is unavailable while this session is paused"
                                            .to_string(),
                                    ),
                                }),
                            )
                            .await?;
                            continue;
                        }
                        if let Err(error) = validate_prepare(
                            &current_role_state,
                            &prepare,
                            controller_fingerprint,
                            local_fingerprint,
                        ) {
                            tracing::debug!(%error, "ignored invalid or stale role prepare");
                            continue;
                        }
                        if local_is_controller {
                            capture_input_active = false;
                            if let Some(capture) = &capture {
                                capture.release(None).await.ok();
                                capture.disarm().await.ok();
                            }
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::input(role_epoch, InputEvent::AllKeysUp),
                            )
                            .await
                            .ok();
                        } else {
                            input_epoch.suspend();
                            backend.all_keys_up().await.ok();
                        }

                        let proposed_local = prepare.controller_fingerprint.as_deref()
                            == Some(local_fingerprint);
                        let mut capture_ready = true;
                        let inject_ready = true;
                        let mut failure_detail = None;
                        if proposed_local && capture.is_none() {
                            match PortalCaptureBackend::preflight(opposite_edge(
                                prepare.listener_position,
                            ))
                            .await
                            {
                                Ok(backend) => capture = Some(backend),
                                Err(error) => {
                                    capture_ready = false;
                                    failure_detail = Some(format!(
                                        "listener capture preflight failed: {error}"
                                    ));
                                }
                            }
                        }
                        prepared_role = Some(prepare.clone());
                        role_request_deadline = Some(
                            tokio::time::Instant::now() + Duration::from_secs(5),
                        );
                        if let Some(tray) = tray {
                            tray.role_switching(true).await;
                        }
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Role(RoleEvent::Ready {
                                role_epoch: prepare.role_epoch,
                                capture_ready,
                                inject_ready,
                                failure_detail,
                            }),
                        )
                        .await?;
                    }
                    Frame::Role(RoleEvent::Commit(commit)) => {
                        let prepared = prepared_role
                            .as_ref()
                            .context("received role commit without a matching prepare")?;
                        if let Err(error) = validate_commit(prepared, &commit) {
                            tracing::debug!(%error, "ignored invalid or stale role commit");
                            continue;
                        }
                        let controller = commit.controller_fingerprint.as_deref()
                            .context("role commit omitted controller identity")?;
                        role_store
                            .save(&CommittedRole::new(controller))
                            .await
                            .context("failed to mirror committed role")?;
                        role_epoch = commit.role_epoch;
                        local_is_controller = controller == local_fingerprint;
                        capture_input_active = false;
                        current_role_state = commit;
                        prepared_role = None;
                        role_request_deadline = None;
                        input_epoch.suspend();
                        backend.all_keys_up().await.ok();
                        if local_is_controller
                            && input_forwarding_enabled
                            && !session_paused
                            && let Some(capture) = &capture
                        {
                            capture.arm().await?;
                        }
                        if let Some(tray) = tray {
                            tray.role_assignment(
                                config.device_name.clone(),
                                controller_name.to_string(),
                                local_is_controller,
                                role_switch_available && !session_paused,
                            )
                            .await;
                        }
                    }
                    Frame::Role(RoleEvent::Abort(abort)) => {
                        let Some(_prepared) = prepared_role.as_ref() else {
                            tracing::debug!("ignored role abort without an active prepare");
                            continue;
                        };
                        if abort.role_epoch != current_role_state.role_epoch
                            || abort.controller_fingerprint
                                != current_role_state.controller_fingerprint
                        {
                            tracing::debug!("ignored stale role abort");
                            continue;
                        }
                        prepared_role = None;
                        role_request_deadline = None;
                        if local_is_controller
                            && input_forwarding_enabled
                            && !session_paused
                            && let Some(capture) = &capture
                        {
                            capture.arm().await?;
                        }
                        if let Some(tray) = tray {
                            tray.role_assignment(
                                config.device_name.clone(),
                                controller_name.to_string(),
                                local_is_controller,
                                role_switch_available && !session_paused,
                            )
                            .await;
                            if let Some(detail) = abort.failure_detail {
                                tray.role_failure(detail).await;
                            }
                        }
                    }
                    Frame::Role(RoleEvent::SetPaused { paused }) => {
                        session_paused = paused;
                        if paused {
                            capture_input_active = false;
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                            audio_receiver = None;
                            input_epoch.suspend();
                            backend.all_keys_up().await.ok();
                            if let Some(capture) = &capture {
                                capture.disarm().await.ok();
                            }
                        } else if local_is_controller && let Some(capture) = &capture {
                            capture.arm().await?;
                        }
                        if let Some(tray) = tray {
                            tray.input_forwarding(input_forwarding_enabled).await;
                            tray.session_paused(paused).await;
                            tray.role_assignment(
                                config.device_name.clone(),
                                controller_name.to_string(),
                                local_is_controller,
                                role_switch_available && !paused,
                            )
                            .await;
                        }
                    }
                    Frame::Role(
                        RoleEvent::Request { .. } | RoleEvent::Ready { .. },
                    ) => {
                        tracing::warn!("ignored connector-only role event from connector");
                    }
                    Frame::ScreenInfo(info) => {
                        tracing::info!(
                            primary = %info.primary_output,
                            outputs = info.outputs.len(),
                            "controller screen info"
                        );
                    }
                    Frame::Hello(_) | Frame::Error(_) | Frame::Pairing(_) => {}
                }
            },
            _ = clipboard_send.tick(), if !session_paused && clipboard_sync.outgoing.is_some() => {
                match clipboard_sync.send_next_image_frame(&mut writer).await {
                    Ok(true) => {
                        tracing::info!("completed Linux clipboard image transfer to controller");
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "failed to send Linux clipboard image chunk");
                        clipboard_sync.outgoing = None;
                    }
                }
            },
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
async fn handle_controller(
    _session: NoiseSession<TcpStream>,
    _config: &AppConfig,
    _config_path: &Path,
    _backend: &ReceiverBackend,
    _tray: Option<&ReceiverTrayHandle>,
    _tray_commands: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
    _log_path: &Path,
    _screen_info: Option<ScreenInfo>,
    _audio_socket: Arc<UdpSocket>,
    _controller_ip: std::net::IpAddr,
    _controller_supports_audio_playback: bool,
    _controller_supports_audio_capture: bool,
    _controller_supports_audio_route: bool,
    _controller_supports_input_toggle: bool,
    _controller_supports_images: bool,
    _controller_supports_input_capture: bool,
    _controller_supports_input_injection: bool,
    _controller_supports_role_switch: bool,
    _controller_fingerprint: &str,
    _local_fingerprint: &str,
    _controller_name: &str,
) -> Result<ControllerSessionExit> {
    anyhow::bail!("Linux receiver sessions are available only on Linux")
}

#[derive(Default)]
struct ReceiverClipboardState {
    tracker: ClipboardChangeTracker,
    outgoing: Option<OutgoingImageTransfer>,
    incoming: IncomingImageTransfer,
    next_transfer_id: u64,
    peer_supports_images: bool,
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<&'static str> {
    tokio::signal::ctrl_c().await?;
    Ok("Ctrl+C")
}

impl ReceiverClipboardState {
    async fn new(config: &AppConfig, peer_supports_images: bool) -> Result<Self> {
        let last_observed = match read_clipboard_item(&config.clipboard).await {
            Ok(item) => item.as_ref().map(ClipboardItem::id),
            Err(error) => {
                tracing::debug!(%error, "skipped initial Linux clipboard observation");
                None
            }
        };
        Ok(Self {
            tracker: ClipboardChangeTracker::new(last_observed),
            outgoing: None,
            incoming: IncomingImageTransfer::default(),
            next_transfer_id: 0,
            peer_supports_images,
        })
    }

    async fn handle_event(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
        event: ClipboardEvent,
    ) -> Result<bool> {
        match event {
            ClipboardEvent::TextOffer { text, .. } => {
                self.handle_text_offer(config, writer, text).await?;
                Ok(true)
            }
            ClipboardEvent::TextRequest => self.send_local_text_offer(config, writer).await,
            ClipboardEvent::ContentRequest => self.send_local_offer(config, writer).await,
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
                    tracing::info!("controller cancelled Linux clipboard image transfer");
                    return Ok(false);
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
                    return Ok(false);
                }
                match self
                    .incoming
                    .handle(event, config.clipboard.max_image_bytes)
                {
                    Ok(Some(image)) => {
                        let remote_id = ClipboardContentId::Image(image.content_sha256);
                        let current = read_clipboard_item(&config.clipboard).await?;
                        let current_id = current.as_ref().map(ClipboardItem::id);
                        if current_id == Some(remote_id) {
                            self.tracker.mark_observed(current_id);
                            return Ok(false);
                        }
                        if current_id.is_some() && !self.tracker.is_observed(&current_id) {
                            self.offer_item(config, writer, current, false).await?;
                            return Ok(false);
                        }
                        write_clipboard_image(&config.clipboard, &image).await?;
                        self.tracker.mark_observed(Some(remote_id));
                        tracing::info!(
                            width = image.width,
                            height = image.height,
                            bytes = image.png.len(),
                            "updated Linux image clipboard from controller"
                        );
                        Ok(true)
                    }
                    Ok(None) => Ok(false),
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
                        tracing::warn!(%error, "rejected controller clipboard image transfer");
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn handle_text_offer(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
        remote_text: String,
    ) -> Result<()> {
        let remote_item = ClipboardItem::Text(remote_text.clone());
        let remote_id = remote_item.id();
        let current = read_clipboard_item(&config.clipboard).await?;
        let current_id = current.as_ref().map(ClipboardItem::id);
        if current_id == Some(remote_id) {
            self.tracker.mark_observed(current_id);
            return Ok(());
        }
        if current_id.is_some() && !self.tracker.is_observed(&current_id) {
            self.offer_item(config, writer, current, false).await?;
            return Ok(());
        }
        write_clipboard_text(&config.clipboard, &remote_text).await?;
        self.tracker.mark_observed(Some(remote_id));
        tracing::info!("updated Linux clipboard from controller");
        Ok(())
    }

    async fn send_local_offer(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
    ) -> Result<bool> {
        let current = read_clipboard_item(&config.clipboard).await?;
        self.offer_item(config, writer, current, true).await
    }

    async fn send_local_text_offer(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
    ) -> Result<bool> {
        let Some(text) = read_clipboard_text(&config.clipboard).await? else {
            return Ok(false);
        };
        let item = ClipboardItem::Text(text.clone());
        let Some(sequence) = self.tracker.offer_current(Some(item.id())) else {
            return Ok(false);
        };
        write_secure_frame_writer(
            writer,
            &Frame::Clipboard(ClipboardEvent::TextOffer { sequence, text }),
        )
        .await?;
        Ok(true)
    }

    async fn send_changed_offer(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
    ) -> Result<bool> {
        let current = read_clipboard_item(&config.clipboard).await?;
        self.offer_item(config, writer, current, false).await
    }

    async fn offer_item(
        &mut self,
        config: &AppConfig,
        writer: &mut ScheduledNoiseWriter,
        current: Option<ClipboardItem>,
        force: bool,
    ) -> Result<bool> {
        let current_id = current.as_ref().map(ClipboardItem::id);
        let sequence = if force {
            self.tracker.offer_current(current_id)
        } else {
            self.tracker.offer_if_changed(current_id)
        };
        let Some(sequence) = sequence else {
            return Ok(false);
        };
        match current {
            Some(ClipboardItem::Text(text)) => {
                if let Some(active) = self.outgoing.take() {
                    write_secure_frame_writer(
                        writer,
                        &Frame::Clipboard(active.cancel_event(ClipboardCancelReason::Replaced)),
                    )
                    .await?;
                }
                write_secure_frame_writer(
                    writer,
                    &Frame::Clipboard(ClipboardEvent::TextOffer { sequence, text }),
                )
                .await?;
                Ok(true)
            }
            Some(ClipboardItem::Image(image))
                if self.peer_supports_images && config.clipboard.images_enabled =>
            {
                if let Some(active) = self.outgoing.take() {
                    write_secure_frame_writer(
                        writer,
                        &Frame::Clipboard(active.cancel_event(ClipboardCancelReason::Replaced)),
                    )
                    .await?;
                }
                self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
                self.outgoing = Some(OutgoingImageTransfer::new(
                    self.next_transfer_id,
                    sequence,
                    image,
                ));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn send_next_image_frame(&mut self, writer: &mut ScheduledNoiseWriter) -> Result<bool> {
        let Some(transfer) = self.outgoing.as_mut() else {
            return Ok(false);
        };
        let Some(event) = transfer.next_event() else {
            self.outgoing = None;
            return Ok(false);
        };
        let completed = matches!(event, ClipboardEvent::ImageEnd { .. });
        write_secure_frame_writer(writer, &Frame::Clipboard(event)).await?;
        if completed {
            self.outgoing = None;
        }
        Ok(completed)
    }
}

#[derive(Default)]
struct ReceiverInputStats {
    motion: u64,
    buttons: u64,
    wheel: u64,
    keys: u64,
    all_keys_up: u64,
    clipboard: u64,
    control: u64,
    return_releases: u64,
    heartbeats: u64,
}

impl ReceiverInputStats {
    fn record_input(&mut self, event: &InputEvent) {
        match event {
            InputEvent::PointerMotion { .. } => {
                self.motion = self.motion.saturating_add(1);
            }
            InputEvent::PointerButton { .. } => {
                self.buttons = self.buttons.saturating_add(1);
            }
            InputEvent::PointerWheel { .. } => {
                self.wheel = self.wheel.saturating_add(1);
            }
            InputEvent::Key { .. } => {
                self.keys = self.keys.saturating_add(1);
            }
            InputEvent::AllKeysUp => {
                self.all_keys_up = self.all_keys_up.saturating_add(1);
            }
        }
    }

    fn log(&self, path: &Path, side: &str) {
        append_portable_log(
            path,
            format!(
                "{side} status motion={} buttons={} wheel={} keys={} all_keys_up={} clipboard={} control={} return_releases={} heartbeats={}",
                self.motion,
                self.buttons,
                self.wheel,
                self.keys,
                self.all_keys_up,
                self.clipboard,
                self.control,
                self.return_releases,
                self.heartbeats
            ),
        );
    }
}

fn should_release_legacy_controller(
    input_forwarding_enabled: bool,
    controller_supports_input_toggle: bool,
    control: &ControlEvent,
) -> bool {
    !input_forwarding_enabled
        && !controller_supports_input_toggle
        && matches!(control, ControlEvent::EnterRemote { .. })
}

struct RemoteReturnWatcher {
    output: Option<OutputInfo>,
    edge: Option<Edge>,
    last_poll: Instant,
    entered_at: Option<Instant>,
    consecutive_edge_polls: u8,
}

impl RemoteReturnWatcher {
    fn new(screen_info: Option<ScreenInfo>) -> Self {
        let output = screen_info.and_then(|info| {
            info.outputs
                .iter()
                .find(|output| output.name == info.primary_output)
                .cloned()
                .or_else(|| info.outputs.first().cloned())
        });

        Self {
            output,
            edge: None,
            last_poll: Instant::now() - RETURN_EDGE_POLL_INTERVAL,
            entered_at: None,
            consecutive_edge_polls: 0,
        }
    }

    fn record_control(&mut self, control: &ControlEvent) {
        match control {
            ControlEvent::EnterRemote { edge, .. } => {
                self.edge = Some(*edge);
                self.last_poll = Instant::now() - RETURN_EDGE_POLL_INTERVAL;
                self.entered_at = Some(Instant::now());
                self.consecutive_edge_polls = 0;
            }
            ControlEvent::ReleaseToLocal { .. } | ControlEvent::LeaveRemote { .. } => {
                self.edge = None;
                self.entered_at = None;
                self.consecutive_edge_polls = 0;
            }
            ControlEvent::SetInputForwarding { enabled: false } => {
                self.edge = None;
                self.entered_at = None;
                self.consecutive_edge_polls = 0;
            }
            ControlEvent::SetInputForwarding { enabled: true } => {}
        }
    }

    async fn release_if_at_edge(&mut self) -> Result<Option<ControlEvent>> {
        let Some(edge) = self.edge else {
            return Ok(None);
        };
        let Some(output) = &self.output else {
            return Ok(None);
        };
        if self
            .entered_at
            .is_some_and(|entered_at| entered_at.elapsed() < RETURN_EDGE_ENTRY_GRACE)
        {
            return Ok(None);
        }
        if self.last_poll.elapsed() < RETURN_EDGE_POLL_INTERVAL {
            return Ok(None);
        }
        self.last_poll = Instant::now();

        let cursor = hyprland_cursor_position().await?;
        if !real_cursor_at_return_edge(cursor, output, edge) {
            self.consecutive_edge_polls = 0;
            return Ok(None);
        }

        self.consecutive_edge_polls = self.consecutive_edge_polls.saturating_add(1);
        if self.consecutive_edge_polls < RETURN_EDGE_CONFIRMATIONS {
            return Ok(None);
        }

        let normalized_position = normalized_perpendicular(
            edge,
            Point {
                x: f64::from(cursor.x),
                y: f64::from(cursor.y),
            },
            Rect {
                x: f64::from(output.x),
                y: f64::from(output.y),
                width: output.width,
                height: output.height,
            },
        );
        self.edge = None;
        self.entered_at = None;
        self.consecutive_edge_polls = 0;
        Ok(Some(ControlEvent::LeaveRemote {
            edge,
            normalized_position,
        }))
    }
}

fn real_cursor_at_return_edge(cursor: HyprCursorPosition, output: &OutputInfo, edge: Edge) -> bool {
    let left = output.x;
    let top = output.y;
    let right = output.x + output.width.saturating_sub(1) as i32;
    let bottom = output.y + output.height.saturating_sub(1) as i32;

    match edge {
        Edge::Left => cursor.x >= right - RETURN_EDGE_MARGIN,
        Edge::Right => cursor.x <= left + RETURN_EDGE_MARGIN,
        Edge::Top => cursor.y >= bottom - RETURN_EDGE_MARGIN,
        Edge::Bottom => cursor.y <= top + RETURN_EDGE_MARGIN,
    }
}

fn spawn_controller_reader(mut reader: TcpFrameReader) -> mpsc::UnboundedReceiver<Result<Frame>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let frame = read_secure_frame_reader(&mut reader)
                .await
                .context("failed to read controller frame");
            let should_stop = frame.is_err();
            if sender.send(frame).is_err() || should_stop {
                break;
            }
        }
    });
    receiver
}

enum TrayCommandEvent {
    Command(TrayCommand),
    Closed,
}

async fn recv_tray_command(
    receiver: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
) -> TrayCommandEvent {
    let Some(command_rx) = receiver.as_mut() else {
        return future::pending().await;
    };

    match command_rx.recv().await {
        Some(command) => TrayCommandEvent::Command(command),
        None => {
            *receiver = None;
            TrayCommandEvent::Closed
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

fn default_config_path() -> PathBuf {
    portable_config_path("receiver.toml")
}

#[derive(Debug, Clone)]
enum ReceiverBackend {
    Libei(LibeiBackend),
    Uinput(UinputBackend),
    Hyprland(HyprlandVirtualInputBackend),
    LogOnly,
}

impl ReceiverBackend {
    fn label(&self) -> &'static str {
        match self {
            Self::Libei(_) => "libei",
            Self::Uinput(_) => "uinput",
            Self::Hyprland(_) => "hyprland",
            Self::LogOnly => "log",
        }
    }

    fn from_config(config: &AppConfig) -> Result<Self> {
        let requested = config.input.inject.backend.to_ascii_lowercase();
        let libei = LibeiBackend::probe();

        match requested.as_str() {
            "auto" => {
                if is_niri_session() {
                    match UinputBackend::connect() {
                        Ok(backend) => {
                            tracing::info!("using uinput input backend for Niri session");
                            return Ok(Self::Uinput(backend));
                        }
                        Err(err) => {
                            tracing::warn!(
                                %err,
                                "failed to initialize Niri-compatible uinput backend; trying other input backends"
                            );
                        }
                    }
                }

                if libei.is_available() {
                    match LibeiBackend::connect() {
                        Ok(backend) => {
                            tracing::info!(
                                pkg_config = backend.pkg_config_name(),
                                version = backend.version().unwrap_or("unknown"),
                                "using libei input backend"
                            );
                            return Ok(Self::Libei(backend));
                        }
                        Err(err) => {
                            tracing::warn!(
                                %err,
                                pkg_config = libei.pkg_config_name(),
                                version = libei.version().unwrap_or("unknown"),
                                "failed to initialize libei; trying Hyprland virtual input backend"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        pkg_config = libei.pkg_config_name(),
                        "libei was not found through pkg-config; trying Hyprland virtual input backend"
                    );
                }

                match HyprlandVirtualInputBackend::connect() {
                    Ok(backend) => {
                        tracing::info!("using Hyprland virtual input backend");
                        Ok(Self::Hyprland(backend))
                    }
                    Err(err) => Err(err).context(
                        "input.backend is \"auto\", but no real input backend could be initialized; \
                         ensure the receiver starts inside the graphical session or set \
                         input.backend = \"log\" explicitly for protocol-only testing",
                    ),
                }
            }
            "hyprland" => {
                let backend = HyprlandVirtualInputBackend::connect().context(
                    "input.backend is \"hyprland\", but Hyprland virtual input initialization failed",
                )?;
                tracing::info!("using Hyprland virtual input backend");
                Ok(Self::Hyprland(backend))
            }
            "uinput" => {
                let backend = UinputBackend::connect().context(
                    "input.backend is \"uinput\", but /dev/uinput initialization failed",
                )?;
                tracing::info!("using Linux uinput backend");
                Ok(Self::Uinput(backend))
            }
            "libei" if libei.is_available() => {
                let backend = LibeiBackend::connect()
                    .context("input.backend is \"libei\", but libei initialization failed")?;
                tracing::info!(
                    pkg_config = backend.pkg_config_name(),
                    version = backend.version().unwrap_or("unknown"),
                    "using libei input backend"
                );
                Ok(Self::Libei(backend))
            }
            "libei" => anyhow::bail!(
                "input.backend is \"libei\", but {} is not available through pkg-config",
                libei.pkg_config_name()
            ),
            "log" | "mock" | "none" => {
                tracing::warn!("using log-only input backend; no local input will be injected");
                Ok(Self::LogOnly)
            }
            other => {
                anyhow::bail!(
                    "unsupported input.backend \"{other}\"; expected auto, uinput, hyprland, libei, or log"
                )
            }
        }
    }

    async fn inject(&self, event: InputEvent) -> Result<()> {
        match self {
            Self::Libei(backend) => backend.inject(event).await.map_err(Into::into),
            Self::Uinput(backend) => backend.inject(event).await.map_err(Into::into),
            Self::Hyprland(backend) => backend.inject(event).await.map_err(Into::into),
            Self::LogOnly => {
                tracing::info!(?event, "received input event");
                Ok(())
            }
        }
    }

    async fn all_keys_up(&self) -> Result<()> {
        match self {
            Self::Libei(backend) => backend.all_keys_up().await.map_err(Into::into),
            Self::Uinput(backend) => backend.all_keys_up().await.map_err(Into::into),
            Self::Hyprland(backend) => backend.all_keys_up().await.map_err(Into::into),
            Self::LogOnly => {
                tracing::info!("received all-keys-up");
                Ok(())
            }
        }
    }
}

fn is_niri_session() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter_map(|value| value.into_string().ok())
        .any(|value| desktop_names_include_niri(&value))
        || std::env::var_os("NIRI_SOCKET").is_some()
}

fn desktop_names_include_niri(value: &str) -> bool {
    value
        .split([':', ';'])
        .any(|desktop| desktop.eq_ignore_ascii_case("niri"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_niri_in_desktop_name_lists() {
        assert!(desktop_names_include_niri("niri"));
        assert!(desktop_names_include_niri("NIRI"));
        assert!(desktop_names_include_niri("niri:GNOME"));
        assert!(!desktop_names_include_niri("Hyprland"));
    }

    #[test]
    fn controller_hello_must_match_noise_identity() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "Main PC".to_string(),
            role: Role::Controller,
            public_key_fingerprint: "actual".to_string(),
            capabilities: Vec::new(),
            extensions: Vec::new(),
            node_capabilities: Vec::new(),
        };
        assert!(validate_controller_hello(&hello, "actual").is_ok());
        assert!(validate_controller_hello(&hello, "different").is_err());
    }

    #[test]
    fn disabled_input_releases_a_legacy_controller_on_each_remote_entry() {
        let enter = ControlEvent::EnterRemote {
            edge: Edge::Left,
            normalized_position: 0.5,
        };

        assert!(should_release_legacy_controller(false, false, &enter));
        assert!(!should_release_legacy_controller(true, false, &enter));
        assert!(!should_release_legacy_controller(false, true, &enter));
        assert!(!should_release_legacy_controller(
            false,
            false,
            &ControlEvent::ReleaseToLocal {
                reason: ReleaseReason::UserRequest,
            },
        ));
    }

    #[test]
    fn linux_release_lands_on_the_matching_local_edge() {
        let screen = ScreenInfo {
            outputs: vec![OutputInfo {
                name: "DP-1".to_string(),
                width: 2560,
                height: 1440,
                scale: 1.25,
                x: -1920,
                y: 0,
            }],
            primary_output: "DP-1".to_string(),
        };

        assert_eq!(
            linux_release_cursor(Some(&screen), Edge::Left, 0.25),
            Some((-1917.0, 360.0))
        );
        assert_eq!(
            linux_release_cursor(Some(&screen), Edge::Right, 0.25),
            Some((636.0, 360.0))
        );
        assert_eq!(
            linux_release_cursor(Some(&screen), Edge::Top, 0.25),
            Some((-1280.0, 3.0))
        );
        assert_eq!(
            linux_release_cursor(Some(&screen), Edge::Bottom, 0.25),
            Some((-1280.0, 1436.0))
        );
    }

    #[test]
    fn empty_capture_output_reuses_the_injection_output() {
        let mut config = AppConfig::receiver_default();
        assert_eq!(linux_capture_output(&config), "eDP-1");
        config.input.capture.output = "DP-2".to_string();
        assert_eq!(linux_capture_output(&config), "DP-2");
        assert_eq!(
            peer_position_to_edge(edge_common::PeerPosition::Top),
            Edge::Top
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn controller_socket_uses_bounded_tcp_user_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connect = TcpStream::connect(address);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let _client = client.unwrap();
        let (server, _) = accepted.unwrap();

        configure_controller_socket(&server).unwrap();

        assert_eq!(
            SockRef::from(&server).tcp_user_timeout().unwrap(),
            Some(CONTROLLER_STALL_TIMEOUT)
        );
    }

    #[tokio::test]
    async fn legacy_clipboard_config_is_migrated_with_images_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receiver.toml");
        tokio::fs::write(
            &path,
            r#"device_name = "Lua"
role = "receiver"
listen = "0.0.0.0:42420"

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
        let path = directory.path().join("receiver.toml");
        tokio::fs::write(
            &path,
            r#"device_name = "Lua"
role = "receiver"
listen = "0.0.0.0:42420"

[clipboard]
enabled = true
text_only = true
images_enabled = false
max_bytes = 1048576
max_image_bytes = 4194304
"#,
        )
        .await
        .unwrap();

        let config = load_or_create_config(&path).await.unwrap();
        let migrated = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!config.clipboard.images_enabled);
        assert!(!migrated.contains("text_only"));
        assert!(migrated.contains("images_enabled = false"));
    }
}
