use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use edge_common::{AppConfig, Role, default_state_dir, init_tracing, portable_config_path};
use edge_crypto::{IdentityKey, NoiseSession, PinDecision, PinStore, accept_noise_session};
use edge_linux_input::{
    LibeiBackend, hyprland_screen_info, read_clipboard_text, write_clipboard_text,
};
use edge_protocol::{
    ClipboardEvent, Frame, Heartbeat, Hello, InputEvent, PROTOCOL_VERSION, RemoteError,
    decode_frame, encode_frame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    time,
};

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
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TestInput {
    Pointer,
    Click,
    Key,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(default_config_path);
    let config = load_or_create_config(&config_path).await?;

    if config.role != Role::Receiver {
        anyhow::bail!(
            "receiver requires role = \"receiver\" in {}",
            config_path.display()
        );
    }

    let backend = LibeiBackend::probe();
    if !backend.is_available() {
        tracing::warn!("libei was not found through pkg-config; input tests will fail closed");
    }

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

    run_receiver(config, args.pair, backend).await
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

async fn run_input_test(backend: &LibeiBackend, test: TestInput) -> Result<()> {
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

async fn run_receiver(config: AppConfig, allow_pairing: bool, backend: LibeiBackend) -> Result<()> {
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

    tracing::info!(
        listen,
        fingerprint = %identity.fingerprint(),
        allow_pairing,
        "receiver listening"
    );

    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!(%addr, "controller connected");

        let (mut session, peer_fingerprint) = match accept_noise_session(stream, &identity).await {
            Ok(session) => session,
            Err(err) => {
                tracing::warn!(%err, "Noise handshake failed");
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
                tracing::warn!(%err, "failed to read Hello");
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
                tracing::warn!(%err, "rejected controller");
                continue;
            }
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

        if let Err(err) = handle_controller(session, &config, &backend).await {
            tracing::warn!(%err, "controller session ended");
        }
        backend.all_keys_up().await.ok();
    }
}

async fn handle_controller(
    mut session: NoiseSession<TcpStream>,
    config: &AppConfig,
    backend: &LibeiBackend,
) -> Result<()> {
    let mut heartbeat_sequence = 0_u64;
    let mut heartbeat = time::interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                heartbeat_sequence += 1;
                write_secure_frame(&mut session, &Frame::Heartbeat(Heartbeat { sequence: heartbeat_sequence })).await?;
            }
            frame = read_secure_frame(&mut session) => {
                match frame? {
                    Frame::Input(InputEvent::AllKeysUp) => backend.all_keys_up().await?,
                    Frame::Input(event) => backend.inject(event).await?,
                    Frame::Clipboard(ClipboardEvent::TextOffer { text, .. }) => {
                        write_clipboard_text(&config.clipboard, &text).await?;
                    }
                    Frame::Clipboard(ClipboardEvent::TextRequest) => {
                        if let Some(text) = read_clipboard_text(&config.clipboard).await? {
                            write_secure_frame(
                                &mut session,
                                &Frame::Clipboard(ClipboardEvent::TextOffer { sequence: 0, text }),
                            ).await?;
                        }
                    }
                    Frame::Heartbeat(_) => {}
                    Frame::Control(control) => tracing::info!(?control, "control event"),
                    Frame::Hello(_) | Frame::ScreenInfo(_) | Frame::Error(_) => {}
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

async fn read_secure_frame(session: &mut NoiseSession<TcpStream>) -> Result<Frame> {
    let payload = session.read_packet().await?;
    Ok(decode_frame(&payload)?)
}

fn default_config_path() -> PathBuf {
    portable_config_path("receiver.toml")
}
