use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use edge_common::{AppConfig, Role, default_state_dir, init_tracing};
use edge_crypto::{IdentityKey, NoiseSession, initiate_noise_session};
use edge_protocol::{Frame, Hello, PROTOCOL_VERSION, encode_frame};
use tokio::net::TcpStream;

#[derive(Debug, Parser)]
#[command(version, about = "Windows controller for edge-kvm")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, help = "Load config and connect without installing hooks")]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
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

    #[cfg(not(windows))]
    {
        if !args.dry_run {
            anyhow::bail!(
                "edge-controller-win must run on Windows; use --dry-run here to validate config/connectivity"
            );
        }
    }

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
        initiate_noise_session(stream, &identity, Some(&peer.pinned_fingerprint))
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
    Ok(())
}

async fn write_secure_frame(session: &mut NoiseSession<TcpStream>, frame: &Frame) -> Result<()> {
    let payload = encode_frame(frame)?;
    session.write_packet(&payload).await?;
    Ok(())
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
    std::env::var_os("EDGE_KVM_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .map(|path| path.join("edge-kvm/controller.toml"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".config/edge-kvm/controller.toml"))
        })
        .unwrap_or_else(|| PathBuf::from("controller.toml"))
}
