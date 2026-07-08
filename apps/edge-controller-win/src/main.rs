#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use edge_common::{AppConfig, Role, default_state_dir, init_tracing, portable_config_path};
use edge_crypto::{IdentityKey, NoiseSession, initiate_noise_session};
use edge_protocol::{
    ClipboardEvent, Frame, Hello, InputEvent, MouseButton, PROTOCOL_VERSION, decode_frame,
    encode_frame,
};
use tokio::{net::TcpStream, time};

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
    Key,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
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
            let connection = match connect_session(&config, &identity).await {
                Ok(mut connection) => {
                    read_initial_frames(&mut connection.session).await?;
                    Some(connection)
                }
                Err(err) => {
                    tracing::warn!(%err, "starting tray without receiver connection");
                    None
                }
            };

            edge_windows_input::install_hooks().context("failed to install Windows hooks")?;
            let status = connection
                .as_ref()
                .map(ControllerConnection::status)
                .unwrap_or_else(|| "Disconnected".to_string());
            tracing::info!(%status, "starting tray loop");
            edge_windows_input::run_tray(&status).context("failed to run tray app")?;
            return Ok(());
        }
    }

    let mut connection = connect_session(&config, &identity).await?;
    read_initial_frames(&mut connection.session).await?;

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

    run_connected(connection).await
}

fn should_run_tray(args: &Args) -> bool {
    args.tray || (!args.dry_run && args.test_input.is_none() && args.test_clipboard_text.is_none())
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

async fn read_initial_frames(session: &mut NoiseSession<TcpStream>) -> Result<()> {
    loop {
        match time::timeout(Duration::from_millis(750), read_secure_frame(session)).await {
            Ok(Ok(Frame::Hello(hello))) => {
                tracing::info!(
                    device = %hello.device_name,
                    fingerprint = %hello.public_key_fingerprint,
                    "receiver hello"
                );
            }
            Ok(Ok(Frame::ScreenInfo(info))) => {
                tracing::info!(
                    primary = %info.primary_output,
                    outputs = info.outputs.len(),
                    "receiver screen info"
                );
                return Ok(());
            }
            Ok(Ok(Frame::Error(err))) => {
                anyhow::bail!("receiver error: {}: {}", err.code, err.message)
            }
            Ok(Ok(frame)) => tracing::debug!(?frame, "initial receiver frame"),
            Ok(Err(err)) => return Err(err).context("failed to read receiver frame"),
            Err(_) => return Ok(()),
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

async fn run_connected(mut connection: ControllerConnection) -> Result<()> {
    tracing::info!(status = %connection.status(), "connected; press Ctrl+C to quit");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                write_secure_frame(&mut connection.session, &Frame::Input(InputEvent::AllKeysUp)).await.ok();
                tracing::info!("shutdown requested");
                return Ok(());
            }
            frame = read_secure_frame(&mut connection.session) => {
                match frame? {
                    Frame::Heartbeat(heartbeat) => tracing::trace!(sequence = heartbeat.sequence, "heartbeat"),
                    Frame::Clipboard(event) => tracing::info!(?event, "clipboard event"),
                    Frame::ScreenInfo(info) => tracing::info!(primary = %info.primary_output, outputs = info.outputs.len(), "screen info"),
                    Frame::Error(err) => anyhow::bail!("receiver error: {}: {}", err.code, err.message),
                    other => tracing::debug!(?other, "receiver frame"),
                }
            }
        }
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

async fn read_secure_frame(session: &mut NoiseSession<TcpStream>) -> Result<Frame> {
    let payload = session.read_packet().await?;
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
