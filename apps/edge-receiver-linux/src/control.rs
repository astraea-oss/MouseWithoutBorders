//! Control socket for the running receiver.
//!
//! `edge-receiver-linux pair-phone`, `unpair-phone` and `list-phones` talk to
//! the running receiver over `$XDG_RUNTIME_DIR/edge-kvm/control.sock` (mode
//! 0600 in a 0700 directory) and never edit pin files themselves. The socket
//! lives in the runtime directory, so the portable config and state are
//! untouched.
//!
//! Protocol: the client sends one command line; the receiver answers with
//! lines of `<keyword> <value>`.
//!
//! ```text
//! pair-phone          → uri <link>, expires <seconds>, then paired <name> | closed
//! unpair-phone <name> → unpaired <name> | not-paired | error <message>
//! list-phones         → phone <name> connected|disconnected …, then end
//! status              → summary <text>, [peer <name>], [ping <ms>], diag <key>=<value> …, then end
//! arm-pairing         → ok | error <message>
//! ```
//!
//! The settings window uses the same commands.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Subcommand;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    time,
};

use crate::{
    AbortOnDropTask, append_portable_log,
    phone::{PairingOffer, PhoneRequest},
    tray::{ReceiverTrayHandle, TrayCommand, TrayStatus},
};

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum ControlCommand {
    /// Show a QR code that pairs a phone with the running receiver.
    PairPhone,
    /// Forget a paired phone and disconnect it.
    UnpairPhone { name: String },
    /// List paired phones.
    ListPhones,
}

pub(crate) fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("edge-kvm").join("control.sock"))
}

/// Starts serving the control socket. Returns `None` (after logging why) if
/// there's no runtime directory or another receiver already serves it.
pub(crate) async fn spawn_server(
    requests: mpsc::UnboundedSender<PhoneRequest>,
    tray: Option<ReceiverTrayHandle>,
    log_path: &Path,
) -> Option<AbortOnDropTask> {
    let Some(path) = socket_path() else {
        append_portable_log(
            log_path,
            "no XDG_RUNTIME_DIR; phone pairing commands are unavailable",
        );
        return None;
    };
    match bind(&path).await {
        Ok(listener) => {
            let log_path = log_path.to_path_buf();
            Some(AbortOnDropTask(tokio::spawn(serve(
                listener, requests, tray, log_path,
            ))))
        }
        Err(error) => {
            tracing::warn!(%error, "control socket unavailable");
            append_portable_log(log_path, format!("control socket unavailable: {error:#}"));
            None
        }
    }
}

async fn bind(path: &Path) -> Result<UnixListener> {
    let dir = path.parent().context("control socket path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            anyhow::bail!("another receiver is already serving {}", path.display());
        }
        // Left behind by a receiver that didn't exit cleanly.
        std::fs::remove_file(path)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn serve(
    listener: UnixListener,
    requests: mpsc::UnboundedSender<PhoneRequest>,
    tray: Option<ReceiverTrayHandle>,
    log_path: PathBuf,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                append_portable_log(
                    &log_path,
                    format!("control socket accept failed: {error:#}"),
                );
                time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let requests = requests.clone();
        let tray = tray.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, requests, tray).await {
                tracing::debug!(%error, "control client ended");
            }
        });
    }
}

async fn handle_client(
    stream: UnixStream,
    requests: mpsc::UnboundedSender<PhoneRequest>,
    tray: Option<ReceiverTrayHandle>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let mut words = line.split_whitespace();
    let response = match (words.next(), words.collect::<Vec<_>>().join(" ")) {
        (Some("pair-phone"), _) => {
            let (reply, offer) = oneshot::channel();
            let (paired_tx, paired) = oneshot::channel();
            requests.send(PhoneRequest::OpenPairing {
                reply,
                paired: paired_tx,
            })?;
            let PairingOffer { uri, expires_in } = offer.await?;
            writer
                .write_all(format!("uri {uri}\nexpires {}\n", expires_in.as_secs()).as_bytes())
                .await?;
            match time::timeout(expires_in, paired).await {
                Ok(Ok(name)) => format!("paired {name}\n"),
                // Replaced by a newer pair-phone, or expired.
                Ok(Err(_)) | Err(_) => "closed\n".to_string(),
            }
        }
        (Some("unpair-phone"), name) if !name.is_empty() => {
            let (reply, result) = oneshot::channel();
            requests.send(PhoneRequest::Unpair {
                name: name.clone(),
                reply,
            })?;
            match result.await? {
                Ok(true) => format!("unpaired {name}\n"),
                Ok(false) => "not-paired\n".to_string(),
                Err(error) => format!("error {error}\n"),
            }
        }
        (Some("list-phones"), _) => {
            let (reply, phones) = oneshot::channel();
            requests.send(PhoneRequest::List { reply })?;
            let mut response = String::new();
            for (name, connected) in phones.await? {
                let state = if connected {
                    "connected"
                } else {
                    "disconnected"
                };
                response.push_str(&format!("phone {name} {state}\n"));
            }
            response + "end\n"
        }
        (Some("status"), _) => match &tray {
            Some(tray) => status_reply(&tray.status().borrow()),
            None => "summary Running\nend\n".to_string(),
        },
        (Some("arm-pairing"), _) => match &tray {
            Some(tray) => {
                tray.command(TrayCommand::ArmPairing);
                "ok\n".to_string()
            }
            None => "error pairing needs the tray; restart without --no-tray\n".to_string(),
        },
        _ => "error unknown command\n".to_string(),
    };
    writer.write_all(response.as_bytes()).await?;
    Ok(())
}

fn status_reply(status: &TrayStatus) -> String {
    let mut reply = format!("summary {}\n", status.summary);
    if let Some(peer) = &status.peer {
        reply.push_str(&format!("peer {peer}\n"));
    }
    if let Some(ping) = status.round_trip_ms {
        reply.push_str(&format!("ping {ping}\n"));
    }
    for (key, value) in &status.diagnostics {
        reply.push_str(&format!("diag {key}={}\n", value.replace('\n', " ")));
    }
    reply + "end\n"
}

/// Runs a control command against the running receiver and prints the result.
pub(crate) async fn run_client(command: ControlCommand) -> Result<()> {
    let path = socket_path().context("XDG_RUNTIME_DIR isn't set")?;
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "can't reach the running receiver at {}; is edge-receiver-linux running?",
            path.display()
        )
    })?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let request = match &command {
        ControlCommand::PairPhone => "pair-phone".to_string(),
        ControlCommand::UnpairPhone { name } => format!("unpair-phone {name}"),
        ControlCommand::ListPhones => "list-phones".to_string(),
    };
    writer.write_all(format!("{request}\n").as_bytes()).await?;

    let mut paired_any = false;
    while let Some(line) = lines.next_line().await? {
        let (keyword, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match keyword {
            "uri" => {
                println!("{}", render_qr(value)?);
                println!("Scan this with HyprMobile, or paste the link into the app:");
                println!("{value}\n");
            }
            "expires" => println!(
                "Waiting up to {} minutes for the phone…",
                value.parse::<u64>().unwrap_or(300) / 60
            ),
            "paired" => println!("Paired {value}."),
            "closed" => anyhow::bail!("pairing closed before a phone paired (expired or replaced)"),
            "unpaired" => println!("Unpaired {value} and disconnected it."),
            "not-paired" => anyhow::bail!("no phone with that name is paired"),
            "phone" => {
                paired_any = true;
                println!("{value}");
            }
            "end" if !paired_any => println!("No phones are paired."),
            "end" => {}
            "error" => anyhow::bail!("{value}"),
            other => anyhow::bail!("unexpected reply from the receiver: {other}"),
        }
    }
    Ok(())
}

/// A QR code drawn with half-block characters, light-on-dark so it scans
/// from a dark terminal.
fn render_qr(data: &str) -> Result<String> {
    use qrcode::{QrCode, render::unicode::Dense1x2};

    let code = QrCode::new(data.as_bytes()).context("pairing link is too long for a QR code")?;
    Ok(code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_links_fit_in_a_terminal_qr_code() {
        let uri = "hyprmobile://pair?v=1&fp=bb73d9ee:6e6f1951:991b0dec:b716843a&port=42420\
                   &token=0123456789abcdef0123456789abcdef&addr=192.168.0.11&addr=192.168.1.5";
        let qr = render_qr(uri).unwrap();
        let width = qr.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(width <= 80, "QR is {width} columns wide");
    }

    #[tokio::test]
    async fn pair_phone_reports_the_link_and_the_paired_phone() {
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(handle_client(server, requests_tx, None));
        let (reader, mut writer) = client.into_split();
        writer.write_all(b"pair-phone\n").await.unwrap();

        let Some(PhoneRequest::OpenPairing { reply, paired }) = requests.recv().await else {
            panic!("expected an OpenPairing request");
        };
        reply
            .send(PairingOffer {
                uri: "hyprmobile://pair?v=1".to_string(),
                expires_in: Duration::from_secs(300),
            })
            .ok();
        paired.send("Pixel".to_string()).unwrap();

        let mut lines = BufReader::new(reader).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            "uri hyprmobile://pair?v=1"
        );
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "expires 300");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "paired Pixel");
    }

    #[test]
    fn status_reply_lists_the_status_then_diagnostics() {
        let status = TrayStatus {
            summary: "Main PC is controlling this computer · 4 ms".to_string(),
            connected: true,
            peer: Some("Main PC".to_string()),
            round_trip_ms: Some(4),
            diagnostics: vec![("last error", "none".to_string())],
        };
        assert_eq!(
            status_reply(&status),
            "summary Main PC is controlling this computer · 4 ms\npeer Main PC\nping 4\ndiag last error=none\nend\n"
        );
    }

    #[tokio::test]
    async fn unknown_commands_are_refused() {
        let (requests_tx, _requests) = mpsc::unbounded_channel();
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(handle_client(server, requests_tx, None));
        let (reader, mut writer) = client.into_split();
        writer.write_all(b"format-disk\n").await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            "error unknown command"
        );
    }
}
