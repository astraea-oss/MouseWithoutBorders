#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::time::Duration;
use std::time::Instant;
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
#[cfg(windows)]
use edge_common::PeerPosition;
use edge_common::{
    AppConfig, Role, default_state_dir, detect_primary_local_ip, init_tracing, portable_config_path,
};
use edge_crypto::{IdentityKey, NoiseReader, NoiseSession, NoiseWriter, initiate_noise_session};
#[cfg(windows)]
use edge_geometry::Size;
#[cfg(windows)]
use edge_protocol::Edge;
use edge_protocol::{
    ClipboardEvent, ControlEvent, Frame, Hello, InputEvent, MouseButton, PROTOCOL_VERSION,
    ScreenInfo, decode_frame, encode_frame,
};
use edge_ui::{PairingUiState, SettingsUiInput};
use tokio::{net::TcpStream, sync::mpsc, time};

#[cfg(windows)]
const LIVE_INPUT_QUEUE_CAPACITY: usize = 32;
#[cfg(windows)]
const LIVE_INPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(8);
const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(10);
const RECEIVER_STALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(version, about = "Windows controller for edge-kvm")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, help = "Load config and connect without installing hooks")]
    dry_run: bool,
    #[arg(long, help = "Run the Windows tray shell after connecting")]
    tray: bool,
    #[arg(long, help = "Send one test input event over the encrypted session")]
    test_input: Option<TestInput>,
    #[arg(
        long,
        help = "Send one text clipboard offer over the encrypted session"
    )]
    test_clipboard_text: Option<String>,
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
    let config = load_or_create_config(&config_path).await?;

    if config.role != Role::Controller {
        anyhow::bail!(
            "controller requires role = \"controller\" in {}",
            config_path.display()
        );
    }

    let identity = IdentityKey::load_or_create(default_state_dir().join("identity.toml"))
        .await
        .context("failed to load controller identity")?;

    #[cfg(windows)]
    {
        if run_tray {
            let mut connection = connect_for_tray(&config, &identity, &controller_log).await;
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
                .unwrap_or_else(|| "Disconnected".to_string());
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

            loop {
                if handle_pending_windows_tray_commands(
                    &mut tray_command_rx,
                    &config_path,
                    &config,
                    &controller_log,
                )? {
                    return Ok(());
                }

                if let Some((active_connection, screen_info)) = connection {
                    update_windows_tray_status(&active_connection.status(), &controller_log);
                    match run_connected(
                        active_connection,
                        &config,
                        screen_info,
                        &controller_log,
                        &config_path,
                        Some(&mut tray_command_rx),
                    )
                    .await
                    {
                        Ok(()) => return Ok(()),
                        Err(err) => {
                            tracing::warn!(%err, "connected session ended; reconnecting");
                            append_portable_log(
                                &controller_log,
                                format!("connected session ended; reconnecting: {err:#}"),
                            );
                            update_windows_tray_status("Disconnected", &controller_log);
                        }
                    }
                }

                time::sleep(Duration::from_secs(2)).await;
                connection = connect_for_tray(&config, &identity, &controller_log).await;
                let status = connection
                    .as_ref()
                    .map(|(connection, _)| connection.status())
                    .unwrap_or_else(|| "Disconnected".to_string());
                update_windows_tray_status(&status, &controller_log);
            }
        }
    }

    let mut connection = connect_session(&config, &identity).await?;
    let screen_info = read_initial_frames(&mut connection.session).await?;

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

    run_connected(
        connection,
        &config,
        screen_info,
        &controller_log,
        &config_path,
        None,
    )
    .await
}

#[cfg(windows)]
fn should_run_tray(args: &Args) -> bool {
    args.tray || (!args.dry_run && args.test_input.is_none() && args.test_clipboard_text.is_none())
}

#[cfg(windows)]
async fn connect_for_tray(
    config: &AppConfig,
    identity: &IdentityKey,
    log_path: &Path,
) -> Option<(ControllerConnection, Option<ScreenInfo>)> {
    match connect_session(config, identity).await {
        Ok(mut connection) => match read_initial_frames(&mut connection.session).await {
            Ok(screen_info) => Some((connection, screen_info)),
            Err(err) => {
                tracing::warn!(%err, "failed to initialize receiver session");
                append_portable_log(
                    log_path,
                    format!("failed to initialize receiver session: {err:#}"),
                );
                None
            }
        },
        Err(err) => {
            tracing::warn!(%err, "tray connection attempt failed");
            append_portable_log(log_path, format!("tray connection attempt failed: {err:#}"));
            None
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
fn handle_pending_windows_tray_commands(
    commands: &mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>,
    config_path: &Path,
    config: &AppConfig,
    log_path: &Path,
) -> Result<bool> {
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
            edge_windows_input::WindowsTrayCommand::ReleaseControl => {
                edge_windows_input::release_to_local(edge_protocol::ReleaseReason::UserRequest);
            }
            edge_windows_input::WindowsTrayCommand::Quit => {
                append_portable_log(log_path, "quit requested from tray");
                return Ok(true);
            }
        }
    }

    Ok(false)
}

#[cfg(windows)]
fn controller_pairing_state(config: &AppConfig) -> PairingUiState {
    if let Some(peer) = &config.peer.laptop
        && !peer.pinned_fingerprint.trim().is_empty()
    {
        return PairingUiState::Paired {
            peer_name: "laptop".to_string(),
            peer_fingerprint: peer.pinned_fingerprint.clone(),
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
    peer_fingerprint: String,
}

impl ControllerConnection {
    fn status(&self) -> String {
        format!("Connected to {} ({})", self.addr, self.peer_fingerprint)
    }
}

async fn connect_session(
    config: &AppConfig,
    identity: &IdentityKey,
) -> Result<ControllerConnection> {
    let peer = config
        .peer
        .laptop
        .as_ref()
        .context("missing [peer.laptop] config")?;
    let addr = format!("{}:{}", peer.host, peer.port);
    let stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("failed to connect to {addr}"))?;
    let (mut session, peer_fingerprint) =
        initiate_noise_session(stream, identity, Some(&peer.pinned_fingerprint))
            .await
            .with_context(|| format!("failed encrypted handshake with {addr}"))?;

    write_secure_frame(
        &mut session,
        &Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: config.device_name.clone(),
            role: Role::Controller,
            public_key_fingerprint: identity.fingerprint(),
        }),
    )
    .await?;

    tracing::info!(%addr, %peer_fingerprint, "sent encrypted controller hello");
    Ok(ControllerConnection {
        session,
        addr,
        peer_fingerprint,
    })
}

async fn read_initial_frames(session: &mut NoiseSession<TcpStream>) -> Result<Option<ScreenInfo>> {
    let mut screen_info = None;
    loop {
        match read_secure_frame(session).await {
            Ok(Frame::Hello(hello)) => {
                tracing::info!(
                    device = %hello.device_name,
                    fingerprint = %hello.public_key_fingerprint,
                    "receiver hello"
                );
            }
            Ok(Frame::ScreenInfo(info)) => {
                tracing::info!(
                    primary = %info.primary_output,
                    outputs = info.outputs.len(),
                    "receiver screen info"
                );
                screen_info = Some(info);
                return Ok(screen_info);
            }
            Ok(Frame::Heartbeat(_)) => return Ok(screen_info),
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
                &Frame::Input(InputEvent::PointerMotion { dx: 80.0, dy: 0.0 }),
            )
            .await?;
        }
        TestInput::Click => {
            write_secure_frame(
                session,
                &Frame::Input(InputEvent::PointerButton {
                    button: MouseButton::Left,
                    down: true,
                }),
            )
            .await?;
            write_secure_frame(
                session,
                &Frame::Input(InputEvent::PointerButton {
                    button: MouseButton::Left,
                    down: false,
                }),
            )
            .await?;
        }
        TestInput::Wheel => {
            write_secure_frame(
                session,
                &Frame::Input(InputEvent::PointerWheel { x: 0.0, y: -1.0 }),
            )
            .await?;
        }
        TestInput::Key => {
            write_secure_frame(
                session,
                &Frame::Input(InputEvent::Key {
                    evdev_code: 30,
                    down: true,
                }),
            )
            .await?;
            write_secure_frame(
                session,
                &Frame::Input(InputEvent::Key {
                    evdev_code: 30,
                    down: false,
                }),
            )
            .await?;
        }
    }

    write_secure_frame(session, &Frame::Input(InputEvent::AllKeysUp)).await?;
    tracing::info!(?test, "sent test input");
    Ok(())
}

async fn run_connected(
    connection: ControllerConnection,
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
    log_path: &Path,
    config_path: &Path,
    tray_commands: Option<&mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>>,
) -> Result<()> {
    let result = run_connected_inner(
        connection,
        config,
        screen_info,
        log_path,
        config_path,
        tray_commands,
    )
    .await;
    #[cfg(windows)]
    edge_windows_input::force_release_to_local();
    result
}

async fn run_connected_inner(
    connection: ControllerConnection,
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
    log_path: &Path,
    config_path: &Path,
    mut tray_commands: Option<&mut mpsc::UnboundedReceiver<edge_windows_input::WindowsTrayCommand>>,
) -> Result<()> {
    tracing::info!(status = %connection.status(), "connected; press Ctrl+C to quit");
    append_portable_log(
        log_path,
        format!("connected session started: {}", connection.status()),
    );
    let mut input_rx = start_live_input(config, screen_info)?;
    let mut live_clipboard = LiveClipboardState::default();
    let mut stats = ControllerInputStats::default();
    let mut status_log = time::interval(STATUS_LOG_INTERVAL);
    let (clipboard_tx, mut clipboard_rx) = mpsc::unbounded_channel();
    let (reader, mut writer) = connection.session.split();
    let mut receiver_rx = spawn_receiver_reader(reader);
    let mut tray_command_poll = time::interval(Duration::from_millis(200));
    let mut connection_watchdog = time::interval(Duration::from_secs(1));
    let mut last_receiver_activity = Instant::now();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                write_secure_frame_writer(&mut writer, &Frame::Input(InputEvent::AllKeysUp)).await.ok();
                tracing::info!("shutdown requested");
                append_portable_log(log_path, "shutdown requested");
                return Ok(());
            },
            _ = status_log.tick() => {
                stats.log(log_path, "controller");
            },
            _ = connection_watchdog.tick() => {
                if last_receiver_activity.elapsed() > RECEIVER_STALL_TIMEOUT {
                    anyhow::bail!(
                        "receiver stopped responding for {:?}; reconnecting",
                        last_receiver_activity.elapsed()
                    );
                }
            },
            _ = tray_command_poll.tick(), if tray_commands.is_some() => {
                if let Some(commands) = tray_commands.as_deref_mut()
                    && handle_pending_windows_tray_commands(commands, config_path, config, log_path)?
                {
                    write_secure_frame_writer(&mut writer, &Frame::Input(InputEvent::AllKeysUp)).await.ok();
                    return Ok(());
                }
            },
            event = recv_live_input(&mut input_rx) => {
                if let Some(frame) = event {
                    if let Some(prefix) = live_clipboard.frame_before_input(&frame, config)? {
                        write_secure_frame_writer(&mut writer, &prefix).await?;
                        stats.record_frame(&prefix);
                    }
                    write_secure_frame_writer(&mut writer, &frame).await?;
                    stats.record_frame(&frame);
                    live_clipboard.after_input_sent(&frame, config, &clipboard_tx);
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
                last_receiver_activity = Instant::now();
                match frame {
                    Frame::Heartbeat(heartbeat) => tracing::trace!(sequence = heartbeat.sequence, "heartbeat"),
                    Frame::Clipboard(event) => handle_remote_clipboard_event(event, config, &mut writer).await?,
                    Frame::ScreenInfo(info) => tracing::info!(primary = %info.primary_output, outputs = info.outputs.len(), "screen info"),
                    Frame::Control(ControlEvent::ReleaseToLocal { reason }) => {
                        if edge_windows_input::handle_receiver_release(reason) {
                            tracing::info!(?reason, "accepted receiver-requested local release");
                            append_portable_log(log_path, format!("accepted receiver release: {reason:?}"));
                        } else {
                            tracing::warn!(?reason, "ignored stale or implausible receiver release");
                            append_portable_log(log_path, format!("ignored receiver release: {reason:?}"));
                        }
                    }
                    Frame::Control(control) => tracing::debug!(?control, "receiver control frame"),
                    Frame::Error(err) => anyhow::bail!("receiver error: {}: {}", err.code, err.message),
                    other => tracing::debug!(?other, "receiver frame"),
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
        match frame {
            Frame::Input(InputEvent::PointerMotion { .. }) => {
                self.motion = self.motion.saturating_add(1);
            }
            Frame::Input(InputEvent::PointerButton { .. }) => {
                self.buttons = self.buttons.saturating_add(1);
            }
            Frame::Input(InputEvent::PointerWheel { .. }) => {
                self.wheel = self.wheel.saturating_add(1);
            }
            Frame::Input(InputEvent::Key { .. }) => {
                self.keys = self.keys.saturating_add(1);
            }
            Frame::Input(InputEvent::AllKeysUp) => {
                self.keys = self.keys.saturating_add(1);
            }
            Frame::Clipboard(_) => {
                self.clipboard = self.clipboard.saturating_add(1);
            }
            Frame::Control(_) => {
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
    mut reader: NoiseReader,
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
    sequence: u64,
}

impl LiveClipboardState {
    fn frame_before_input(&mut self, frame: &Frame, config: &AppConfig) -> Result<Option<Frame>> {
        #[cfg(windows)]
        {
            if !config.clipboard.enabled {
                return Ok(None);
            }

            if matches!(
                frame,
                Frame::Input(InputEvent::Key {
                    evdev_code: 47,
                    down: true
                })
            ) && self.ctrl_down
                && let Some(text) =
                    edge_windows_input::read_clipboard_text(config.clipboard.max_bytes)
                        .context("failed to read Windows clipboard")?
            {
                self.sequence = self.sequence.saturating_add(1);
                return Ok(Some(Frame::Clipboard(ClipboardEvent::TextOffer {
                    sequence: self.sequence,
                    text,
                })));
            }
        }

        let _ = (frame, config);
        Ok(None)
    }

    fn after_input_sent(
        &mut self,
        frame: &Frame,
        config: &AppConfig,
        clipboard_tx: &mpsc::UnboundedSender<Frame>,
    ) {
        #[cfg(windows)]
        {
            match frame {
                Frame::Input(InputEvent::Key { evdev_code, down }) => match *evdev_code {
                    29 | 97 => {
                        self.ctrl_down = *down;
                    }
                    46 if *down && self.ctrl_down && config.clipboard.enabled => {
                        let clipboard_tx = clipboard_tx.clone();
                        tokio::spawn(async move {
                            time::sleep(Duration::from_millis(200)).await;
                            let _ =
                                clipboard_tx.send(Frame::Clipboard(ClipboardEvent::TextRequest));
                        });
                    }
                    _ => {}
                },
                Frame::Input(InputEvent::AllKeysUp) => {
                    self.ctrl_down = false;
                }
                _ => {}
            }
        }

        let _ = (frame, config, clipboard_tx);
    }
}

async fn handle_remote_clipboard_event(
    event: ClipboardEvent,
    config: &AppConfig,
    writer: &mut NoiseWriter,
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
                edge_windows_input::write_clipboard_text(&text, config.clipboard.max_bytes)
                    .context("failed to write Windows clipboard")?;
                tracing::info!("updated Windows clipboard from receiver");
            }
            ClipboardEvent::TextRequest => {
                if let Some(text) =
                    edge_windows_input::read_clipboard_text(config.clipboard.max_bytes)
                        .context("failed to read Windows clipboard")?
                {
                    write_secure_frame_writer(
                        writer,
                        &Frame::Clipboard(ClipboardEvent::TextOffer { sequence: 0, text }),
                    )
                    .await?;
                    tracing::info!("sent Windows clipboard to receiver");
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (config, writer);
        tracing::info!(?event, "clipboard event");
    }

    Ok(())
}

#[cfg(windows)]
fn start_live_input(
    config: &AppConfig,
    screen_info: Option<ScreenInfo>,
) -> Result<Option<mpsc::Receiver<Frame>>> {
    let peer = config
        .peer
        .laptop
        .as_ref()
        .context("missing [peer.laptop] config")?;
    let Some(remote_size) = remote_size(screen_info.as_ref()) else {
        tracing::warn!("receiver did not provide screen info; live edge capture disabled");
        return Ok(None);
    };
    let capture = edge_windows_input::start_capture(edge_windows_input::CaptureConfig {
        edge: peer_position_to_edge(peer.position),
        remote_size,
        game_compatibility: config.input.game_compatibility,
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
        edge_windows_input::CapturedInput::Input(event) => Frame::Input(event),
        edge_windows_input::CapturedInput::Control(event) => Frame::Control(event),
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
        if let Frame::Input(InputEvent::PointerMotion { dx, dy }) = frame {
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
        let frame = Frame::Input(InputEvent::PointerMotion {
            dx: self.dx,
            dy: self.dy,
        });
        self.dx = 0.0;
        self.dy = 0.0;
        match sender.try_send(frame) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

#[cfg(windows)]
fn remote_size(screen_info: Option<&ScreenInfo>) -> Option<Size> {
    let info = screen_info?;
    let output = info
        .outputs
        .iter()
        .find(|output| output.name == info.primary_output)
        .or_else(|| info.outputs.first())?;
    Some(Size {
        width: output.width,
        height: output.height,
    })
}

#[cfg(windows)]
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

async fn write_secure_frame_writer(writer: &mut NoiseWriter, frame: &Frame) -> Result<()> {
    let payload = encode_frame(frame)?;
    writer.write_packet(&payload).await?;
    Ok(())
}

async fn read_secure_frame(session: &mut NoiseSession<TcpStream>) -> Result<Frame> {
    let payload = session.read_packet().await?;
    Ok(decode_frame(&payload)?)
}

async fn read_secure_frame_reader(reader: &mut NoiseReader) -> Result<Frame> {
    let payload = reader.read_packet().await?;
    Ok(decode_frame(&payload)?)
}

async fn load_or_create_config(path: &PathBuf) -> Result<AppConfig> {
    match AppConfig::load(path).await {
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
