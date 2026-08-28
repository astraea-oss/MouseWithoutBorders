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
    AppConfig, AudioLocalPlayback, Role, TransportMode, default_state_dir, detect_primary_local_ip,
    init_tracing, portable_config_path,
};
use edge_crypto::{
    IdentityKey, NoiseSession, PinStatus, PinStore, accept_noise_session, pairing_code,
};
use edge_linux_input::{
    ClipboardChangeWatcher, HyprCursorPosition, HyprlandVirtualInputBackend, LibeiBackend,
    UinputBackend, hyprland_cursor_position, hyprland_screen_info, read_clipboard_item,
    read_clipboard_text, spawn_clipboard_change_watcher, write_clipboard_image,
    write_clipboard_text,
};
use edge_protocol::{
    AudioCodec, AudioControl, AudioStreamState, CLIPBOARD_IMAGE_EXTENSION, ClipboardCancelReason,
    ClipboardEvent, ControlEvent, Edge, Frame, Heartbeat, Hello, INITIAL_ROLE_EPOCH,
    INPUT_TOGGLE_EXTENSION, InputEvent, NodeCapability, OutputInfo, PAIRING_CONFIRMATION_EXTENSION,
    PROTOCOL_VERSION, PairingEvent, ReleaseReason, RemoteError, RoleEvent, ScreenInfo,
    decode_frame, encode_frame,
};
use edge_runtime::{
    InputEpochGate, LivenessConfig, LivenessEvent, LivenessTracker, SecureFrameReader,
    SecureFrameSession, SecureFrameWriter,
};
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
use tray::{ReceiverTrayHandle, TrayCommand};

#[derive(Debug, Parser)]
#[command(version, about = "Linux receiver daemon for edge-kvm")]
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

    if config.transport != TransportMode::Listen {
        anyhow::bail!(
            "receiver requires transport = \"listen\" in {}",
            config_path.display()
        );
    }

    if let Err(error) = edge_linux_audio::recover_portable_routing(&default_state_dir()).await {
        tracing::warn!(%error, "failed to recover previous Linux audio routing");
        append_portable_log(
            &receiver_log,
            format!("failed to recover previous Linux audio routing: {error:#}"),
        );
    }

    if args.settings {
        let settings_input = SettingsUiInput {
            role: Role::Receiver,
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

    let backend = ReceiverBackend::from_config(&config)?;

    if let Some(test) = args.test_input {
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
            listen.clone(),
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
                    TrayCommandEvent::Command(TrayCommand::ToggleAudio) => {
                        tracing::info!("audio toggle ignored while no controller is connected");
                    }
                    TrayCommandEvent::Command(TrayCommand::ToggleInputForwarding) => {
                        tracing::info!(
                            "input forwarding toggle ignored while no controller is connected"
                        );
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
                ],
                node_capabilities: vec![
                    NodeCapability::InputCaptureV1,
                    NodeCapability::InputInjectV1,
                    NodeCapability::ScreenInfoBothSidesV1,
                    NodeCapability::AudioCaptureV1,
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
                        "pairing was declined on the controller"
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
                tracing::info!(fingerprint = %peer_fingerprint, "paired controller after two-sided confirmation");
                append_portable_log(
                    &log_path,
                    format!(
                        "paired controller {} ({peer_fingerprint}) after confirmation on both computers",
                        hello.device_name
                    ),
                );
            }
        } else if !controller_trusted {
            let message =
                "controller does not support two-sided pairing confirmation; update it first";
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
        let controller_supports_audio = hello
            .node_capabilities
            .contains(&NodeCapability::AudioPlaybackV1);
        let controller_supports_input_toggle = hello
            .extensions
            .iter()
            .any(|extension| extension == INPUT_TOGGLE_EXTENSION);
        let controller_supports_images = hello
            .extensions
            .iter()
            .any(|extension| extension == CLIPBOARD_IMAGE_EXTENSION);

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
            controller_supports_audio,
            controller_supports_input_toggle,
            controller_supports_images,
            &peer_fingerprint,
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
            "Upgrade the other computer: controller protocol version {} is incompatible with {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        );
    }
    if hello.public_key_fingerprint != noise_fingerprint {
        anyhow::bail!("controller hello fingerprint does not match its encrypted identity");
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
            anyhow::bail!("controller error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected controller pairing status, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read controller pairing status"),
        Err(_) => anyhow::bail!("timed out waiting for controller pairing status"),
    }
}

async fn read_pairing_decision(session: &mut NoiseSession<TcpStream>) -> Result<bool> {
    match time::timeout(Duration::from_secs(120), read_secure_frame(session)).await {
        Ok(Ok(Frame::Pairing(PairingEvent::Decision { accepted }))) => Ok(accepted),
        Ok(Ok(Frame::Error(error))) => {
            anyhow::bail!("controller error: {}: {}", error.code, error.message)
        }
        Ok(Ok(frame)) => anyhow::bail!("expected controller pairing decision, got {frame:?}"),
        Ok(Err(error)) => Err(error).context("failed to read controller pairing decision"),
        Err(_) => anyhow::bail!("timed out waiting for pairing confirmation on the controller"),
    }
}

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
    controller_supports_audio: bool,
    controller_supports_input_toggle: bool,
    controller_supports_images: bool,
    controller_fingerprint: &str,
) -> Result<ControllerSessionExit> {
    let mut heartbeat_sequence = 0_u64;
    let liveness_config = LivenessConfig::default();
    let mut heartbeat = time::interval(liveness_config.heartbeat_interval(true));
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut connection_watchdog = time::interval(Duration::from_millis(250));
    connection_watchdog.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut controller_liveness =
        LivenessTracker::new(liveness_config, tokio::time::Instant::now());
    let mut input_epoch = InputEpochGate::default();
    let mut status_log = time::interval(STATUS_LOG_INTERVAL);
    let mut stats = ReceiverInputStats::default();
    let (reader, mut writer) = SecureFrameSession::new(session).split();
    let mut frame_rx = spawn_controller_reader(reader);
    let mut return_watcher = RemoteReturnWatcher::new(screen_info);
    let mut clipboard_sync =
        ReceiverClipboardState::new(config, controller_supports_images).await?;
    let mut clipboard_send = time::interval(Duration::from_millis(2));
    clipboard_send.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut clipboard_watcher = config
        .clipboard
        .enabled
        .then(spawn_clipboard_change_watcher);
    let mut audio_sender: Option<edge_linux_audio::LinuxAudioSender> = None;
    let (audio_start_tx, mut audio_start_rx) = mpsc::unbounded_channel::<AudioStartResult>();
    let mut audio_start_task: Option<AbortOnDropTask> = None;
    let mut audio_start_generation = 0_u64;
    let mut audio_requested = config.audio.enabled;
    let mut input_forwarding_enabled = true;
    let mut role_epoch = INITIAL_ROLE_EPOCH;
    if let Some(tray) = tray {
        tray.input_forwarding(true).await;
    }

    loop {
        if writer.bulk_is_due() && clipboard_sync.outgoing.is_some() {
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
                match controller_liveness.poll(tokio::time::Instant::now()) {
                    Some(LivenessEvent::SoftInputTimeout) => {
                        input_epoch.suspend();
                        backend.all_keys_up().await.ok();
                        tracing::warn!("controller was silent for one second; released injected input and suspended its input epoch");
                        append_portable_log(log_path, "controller liveness soft timeout; released injected input and suspended stale frames");
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
                            "controller stopped responding for {:?}; closing session",
                            controller_liveness.elapsed(tokio::time::Instant::now())
                        );
                    }
                    None => {}
                }
            }
            _ = heartbeat.tick() => {
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
                        return Ok(ControllerSessionExit::QuitRequested);
                    }
                    TrayCommandEvent::Command(TrayCommand::Disconnect) => {
                        drop(audio_start_task.take());
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
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
                    TrayCommandEvent::Command(TrayCommand::Reconnect) => {}
                    TrayCommandEvent::Command(TrayCommand::ToggleInputForwarding) => {
                        input_forwarding_enabled = !input_forwarding_enabled;
                        if !input_forwarding_enabled {
                            backend.all_keys_up().await.ok();
                            return_watcher.record_control(
                                &ControlEvent::SetInputForwarding { enabled: false },
                            );
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
                    TrayCommandEvent::Command(TrayCommand::ToggleAudio) => {
                        if !controller_supports_audio {
                            tracing::warn!("audio toggle ignored because controller lacks AudioV1");
                            append_portable_log(
                                log_path,
                                "audio toggle ignored because controller lacks AudioV1",
                            );
                            continue;
                        }
                        audio_requested = !audio_requested;
                        persist_receiver_audio_enabled(
                            config_path,
                            config,
                            audio_requested,
                            log_path,
                        )
                        .await;
                        if !audio_requested {
                            drop(audio_start_task.take());
                            audio_start_generation = audio_start_generation.wrapping_add(1);
                            if let Some(sender) = audio_sender.take() {
                                sender.stop().await.ok();
                            }
                        }
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::SetEnabled {
                                enabled: audio_requested,
                            }),
                        )
                        .await?;
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
                        let event = input.event;
                        if event == InputEvent::AllKeysUp {
                            stats.all_keys_up = stats.all_keys_up.saturating_add(1);
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
                                    backend.all_keys_up().await.ok();
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
                        input_epoch.observe_control(&control);
                        stats.control = stats.control.saturating_add(1);
                        return_watcher.record_control(&control);
                        tracing::info!(?control, "control event");
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
                        append_portable_log(log_path, "received Windows audio start request");
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
                    Frame::Audio(AudioControl::SetEnabled { enabled: false }) => {
                        audio_requested = false;
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        persist_receiver_audio_enabled(config_path, config, false, log_path).await;
                        append_portable_log(log_path, "Linux audio streaming disabled");
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Disabled,
                                detail: None,
                            }),
                        )
                        .await?;
                    }
                    Frame::Audio(AudioControl::Stop { reason }) => {
                        drop(audio_start_task.take());
                        audio_start_generation = audio_start_generation.wrapping_add(1);
                        if let Some(sender) = audio_sender.take() {
                            sender.stop().await.ok();
                        }
                        append_portable_log(log_path, format!("Linux audio stopped by controller: {reason:?}"));
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::State {
                                state: AudioStreamState::Disabled,
                                detail: None,
                            }),
                        )
                        .await?;
                    }
                    Frame::Audio(AudioControl::SetEnabled { enabled: true }) => {
                        audio_requested = true;
                        persist_receiver_audio_enabled(config_path, config, true, log_path).await;
                        append_portable_log(log_path, "Linux audio streaming enabled; sending UDP offer");
                        write_secure_frame_writer(
                            &mut writer,
                            &Frame::Audio(AudioControl::Offer {
                                udp_port: audio_socket.local_addr()?.port(),
                                codecs: vec![AudioCodec::PcmS16Stereo48Khz],
                            }),
                        )
                        .await?;
                    }
                    Frame::Audio(AudioControl::State { .. } | AudioControl::Offer { .. }) => {}
                    Frame::Role(RoleEvent::SessionState(state)) => {
                        if state.controller_fingerprint.as_deref()
                            != Some(controller_fingerprint)
                        {
                            anyhow::bail!(
                                "connector role state names a controller other than its authenticated identity"
                            );
                        }
                        if state.role_epoch == 0 {
                            anyhow::bail!("connector sent invalid zero role epoch");
                        }
                        role_epoch = state.role_epoch;
                        input_forwarding_enabled = !state.paused;
                        if state.paused {
                            input_epoch.suspend();
                            backend.all_keys_up().await.ok();
                        }
                        if let Some(tray) = tray {
                            tray.input_forwarding(input_forwarding_enabled).await;
                        }
                        tracing::info!(
                            role_epoch,
                            listener_position = ?state.listener_position,
                            paused = state.paused,
                            "adopted connector-authoritative session state"
                        );
                    }
                    Frame::Role(event) => {
                        tracing::warn!(?event, "role transition ignored until role switching is enabled");
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
            _ = clipboard_send.tick(), if clipboard_sync.outgoing.is_some() => {
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

async fn persist_receiver_audio_enabled(
    config_path: &Path,
    fallback: &AppConfig,
    enabled: bool,
    log_path: &Path,
) {
    let mut updated = match AppConfig::load(config_path).await {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "failed to reload receiver config before saving audio state");
            fallback.clone()
        }
    };
    updated.audio.enabled = enabled;
    if let Err(error) = updated.save(config_path).await {
        tracing::warn!(%error, "failed to persist receiver audio state");
        append_portable_log(
            log_path,
            format!("failed to persist receiver audio state enabled={enabled}: {error}"),
        );
    }
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

        self.edge = None;
        self.entered_at = None;
        self.consecutive_edge_polls = 0;
        Ok(Some(ControlEvent::ReleaseToLocal {
            reason: ReleaseReason::UserRequest,
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
