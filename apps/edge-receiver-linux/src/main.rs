use std::{
    fs::OpenOptions,
    future,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use edge_common::{AppConfig, Role, default_state_dir, init_tracing, portable_config_path};
use edge_crypto::{
    IdentityKey, NoiseReader, NoiseSession, NoiseWriter, PinDecision, PinStore,
    accept_noise_session,
};
use edge_linux_input::{
    HyprlandVirtualInputBackend, LibeiBackend, hyprland_screen_info, read_clipboard_text,
    write_clipboard_text,
};
use edge_protocol::{
    ClipboardEvent, Frame, Heartbeat, Hello, InputEvent, PROTOCOL_VERSION, RemoteError,
    decode_frame, encode_frame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time,
};

const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(10);

mod tray;
use tray::{ReceiverTrayHandle, TrayCommand};

#[derive(Debug, Parser)]
#[command(version, about = "Linux receiver daemon for edge-kvm")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, help = "Allow one unpinned controller to pair")]
    pair: bool,
    #[arg(long)]
    test_input: Option<TestInput>,
    #[arg(long)]
    test_clipboard: bool,
    #[arg(long, help = "Disable the StatusNotifier tray item")]
    no_tray: bool,
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
    let receiver_log = default_state_dir().join("receiver.log");
    install_receiver_panic_log(receiver_log.clone());
    append_portable_log(&receiver_log, "receiver process starting");

    let result = run_main(receiver_log.clone()).await;
    match &result {
        Ok(()) => append_portable_log(&receiver_log, "receiver process exited cleanly"),
        Err(err) => append_portable_log(
            &receiver_log,
            format!("receiver process exited with error: {err:#}"),
        ),
    }
    result
}

async fn run_main(receiver_log: PathBuf) -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(default_config_path);
    let config = load_or_create_config(&config_path).await?;

    if config.role != Role::Receiver {
        anyhow::bail!(
            "receiver requires role = \"receiver\" in {}",
            config_path.display()
        );
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

    run_receiver(config, args.pair, backend, !args.no_tray, receiver_log).await
}

async fn load_or_create_config(path: &PathBuf) -> Result<AppConfig> {
    match AppConfig::load(path).await {
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
    allow_pairing: bool,
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
        match ReceiverTrayHandle::spawn(listen.clone(), backend.label().to_string(), allow_pairing)
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
        allow_pairing,
        "receiver listening"
    );
    append_portable_log(
        &log_path,
        format!(
            "receiver listening on {listen}; fingerprint={}; allow_pairing={allow_pairing}",
            identity.fingerprint()
        ),
    );

    loop {
        let (stream, addr) = tokio::select! {
            command = recv_tray_command(&mut tray_commands) => {
                if matches!(command, Some(TrayCommand::Quit)) {
                    tracing::info!("quit requested from tray");
                    append_portable_log(&log_path, "quit requested from tray");
                    break;
                }
                continue;
            }
            incoming = listener.accept() => incoming?,
        };
        tracing::info!(%addr, "controller connected");
        append_portable_log(&log_path, format!("controller connected: {addr}"));

        let (mut session, peer_fingerprint) = match accept_noise_session(stream, &identity).await {
            Ok(session) => session,
            Err(err) => {
                if let Some(tray) = &tray {
                    tray.error(format!("Noise handshake failed: {err}")).await;
                }
                tracing::warn!(%err, "Noise handshake failed");
                append_portable_log(&log_path, format!("Noise handshake failed: {err:#}"));
                continue;
            }
        };

        let hello = match read_secure_frame(&mut session).await {
            Ok(Frame::Hello(hello)) => hello,
            Ok(other) => {
                tracing::warn!(?other, "first frame was not Hello");
                continue;
            }
            Err(err) => {
                if let Some(tray) = &tray {
                    tray.error(format!("failed to read Hello: {err}")).await;
                }
                tracing::warn!(%err, "failed to read Hello");
                append_portable_log(&log_path, format!("failed to read Hello: {err:#}"));
                continue;
            }
        };

        match pins.verify_or_pin(
            hello.device_name.clone(),
            peer_fingerprint.clone(),
            allow_pairing,
        ) {
            Ok(PinDecision::PinnedNewPeer { fingerprint }) => {
                pins.save(state_dir.join("pins.toml")).await?;
                tracing::info!(%fingerprint, "paired new controller");
                append_portable_log(&log_path, format!("paired new controller: {fingerprint}"));
            }
            Ok(PinDecision::Accepted) => {}
            Err(err) => {
                write_secure_frame(
                    &mut session,
                    &Frame::Error(RemoteError {
                        code: "pin_mismatch".to_string(),
                        message: err.to_string(),
                    }),
                )
                .await
                .ok();
                if let Some(tray) = &tray {
                    tray.error(err.to_string()).await;
                }
                tracing::warn!(%err, "rejected controller");
                append_portable_log(&log_path, format!("rejected controller: {err:#}"));
                continue;
            }
        }

        if let Some(tray) = &tray {
            tray.connected(format!("{} ({peer_fingerprint})", hello.device_name))
                .await;
        }

        write_secure_frame(
            &mut session,
            &Frame::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: config.device_name.clone(),
                role: Role::Receiver,
                public_key_fingerprint: identity.fingerprint(),
            }),
        )
        .await?;

        if let Some(monitor) = config.monitor.as_deref() {
            match hyprland_screen_info(monitor).await {
                Ok(info) => write_secure_frame(&mut session, &Frame::ScreenInfo(info)).await?,
                Err(err) => tracing::warn!(%err, "failed to query Hyprland monitor geometry"),
            }
        }

        match handle_controller(
            session,
            &config,
            &backend,
            tray.as_ref(),
            &mut tray_commands,
            &log_path,
        )
        .await
        {
            Ok(ControllerSessionExit::QuitRequested) => {
                tracing::info!("quit requested from tray");
                append_portable_log(&log_path, "quit requested from tray");
                break;
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

enum ControllerSessionExit {
    QuitRequested,
}

async fn handle_controller(
    session: NoiseSession<TcpStream>,
    config: &AppConfig,
    backend: &ReceiverBackend,
    tray: Option<&ReceiverTrayHandle>,
    tray_commands: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
    log_path: &Path,
) -> Result<ControllerSessionExit> {
    let mut heartbeat_sequence = 0_u64;
    let mut heartbeat = time::interval(Duration::from_millis(250));
    let mut status_log = time::interval(STATUS_LOG_INTERVAL);
    let mut stats = ReceiverInputStats::default();
    let (reader, mut writer) = session.split();
    let mut frame_rx = spawn_controller_reader(reader);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                heartbeat_sequence += 1;
                write_secure_frame_writer(&mut writer, &Frame::Heartbeat(Heartbeat { sequence: heartbeat_sequence })).await?;
                stats.heartbeats = stats.heartbeats.saturating_add(1);
            }
            _ = status_log.tick() => {
                stats.log(log_path, "receiver");
            }
            command = recv_tray_command(tray_commands) => {
                if matches!(command, Some(TrayCommand::Quit)) {
                    return Ok(ControllerSessionExit::QuitRequested);
                }
            }
            frame = frame_rx.recv() => {
                let frame = frame.context("controller frame reader ended")??;
                match frame {
                    Frame::Input(InputEvent::AllKeysUp) => {
                        stats.all_keys_up = stats.all_keys_up.saturating_add(1);
                        backend.all_keys_up().await?;
                        if let Some(tray) = tray {
                            tray.input_event().await;
                        }
                    }
                    Frame::Input(event) => {
                        stats.record_input(&event);
                        backend.inject(event).await?;
                        if let Some(tray) = tray {
                            tray.input_event().await;
                        }
                    }
                    Frame::Clipboard(ClipboardEvent::TextOffer { text, .. }) => {
                        stats.clipboard = stats.clipboard.saturating_add(1);
                        write_clipboard_text(&config.clipboard, &text).await?;
                        if let Some(tray) = tray {
                            tray.clipboard_event().await;
                        }
                    }
                    Frame::Clipboard(ClipboardEvent::TextRequest) => {
                        stats.clipboard = stats.clipboard.saturating_add(1);
                        if let Some(text) = read_clipboard_text(&config.clipboard).await? {
                            write_secure_frame_writer(
                                &mut writer,
                                &Frame::Clipboard(ClipboardEvent::TextOffer { sequence: 0, text }),
                            ).await?;
                            if let Some(tray) = tray {
                                tray.clipboard_event().await;
                            }
                        }
                    }
                    Frame::Heartbeat(_) => {}
                    Frame::Control(control) => {
                        stats.control = stats.control.saturating_add(1);
                        tracing::info!(?control, "control event");
                    }
                    Frame::Hello(_) | Frame::ScreenInfo(_) | Frame::Error(_) => {}
                }
            }
        }
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
                "{side} status motion={} buttons={} wheel={} keys={} all_keys_up={} clipboard={} control={} heartbeats={}",
                self.motion,
                self.buttons,
                self.wheel,
                self.keys,
                self.all_keys_up,
                self.clipboard,
                self.control,
                self.heartbeats
            ),
        );
    }
}

fn spawn_controller_reader(mut reader: NoiseReader) -> mpsc::UnboundedReceiver<Result<Frame>> {
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

async fn recv_tray_command(
    receiver: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
) -> Option<TrayCommand> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => future::pending().await,
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

fn default_config_path() -> PathBuf {
    portable_config_path("receiver.toml")
}

#[derive(Debug, Clone)]
enum ReceiverBackend {
    Libei(LibeiBackend),
    Hyprland(HyprlandVirtualInputBackend),
    LogOnly,
}

impl ReceiverBackend {
    fn label(&self) -> &'static str {
        match self {
            Self::Libei(_) => "libei",
            Self::Hyprland(_) => "hyprland",
            Self::LogOnly => "log",
        }
    }

    fn from_config(config: &AppConfig) -> Result<Self> {
        let requested = config.input.backend.to_ascii_lowercase();
        let libei = LibeiBackend::probe();

        match requested.as_str() {
            "auto" => {
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
                    Err(err) => {
                        tracing::warn!(
                            %err,
                            "failed to initialize Hyprland virtual input backend; using log-only input backend for testing"
                        );
                        Ok(Self::LogOnly)
                    }
                }
            }
            "hyprland" => {
                let backend = HyprlandVirtualInputBackend::connect().context(
                    "input.backend is \"hyprland\", but Hyprland virtual input initialization failed",
                )?;
                tracing::info!("using Hyprland virtual input backend");
                Ok(Self::Hyprland(backend))
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
                    "unsupported input.backend \"{other}\"; expected auto, hyprland, libei, or log"
                )
            }
        }
    }

    async fn inject(&self, event: InputEvent) -> Result<()> {
        match self {
            Self::Libei(backend) => backend.inject(event).await.map_err(Into::into),
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
            Self::Hyprland(backend) => backend.all_keys_up().await.map_err(Into::into),
            Self::LogOnly => {
                tracing::info!("received all-keys-up");
                Ok(())
            }
        }
    }
}
