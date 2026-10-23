//! The settings and phone pairing windows' view of the running receiver,
//! over the control socket (see `control`). Blocking, for the UI thread, with
//! short timeouts so a stuck receiver can't freeze a window.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result};
use edge_ui::{NodeLink, NodeStatus, PhoneEntry, PhonePairingEvent};

use crate::control::socket_path;

const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
/// The user unit installed by `scripts/install-linux-receiver.sh`.
const SERVICE: &str = "edge-kvm-receiver.service";

pub(crate) struct ControlLink {
    log_path: PathBuf,
}

impl ControlLink {
    pub fn new(log_path: PathBuf) -> Self {
        Self { log_path }
    }

    fn connect(&self, command: &str) -> Result<UnixStream> {
        let path = socket_path().context("XDG_RUNTIME_DIR isn't set")?;
        let mut stream = UnixStream::connect(&path).context("edge-kvm isn't running")?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
        stream.write_all(format!("{command}\n").as_bytes())?;
        Ok(stream)
    }

    /// Sends one command and returns every reply line.
    fn request(&self, command: &str) -> Result<Vec<String>> {
        let stream = self.connect(command)?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        BufReader::new(stream)
            .lines()
            .collect::<std::io::Result<_>>()
            .context("no reply from edge-kvm")
    }
}

fn split(line: &str) -> (&str, &str) {
    line.split_once(' ').unwrap_or((line, ""))
}

fn parse_status(lines: &[String]) -> NodeStatus {
    let mut status = NodeStatus::default();
    for line in lines {
        match split(line) {
            ("summary", summary) => status.summary = summary.to_string(),
            ("peer", peer) => {
                status.peer = Some(peer.to_string());
                status.connected = true;
            }
            ("ping", ms) => status.round_trip_ms = ms.parse().ok(),
            ("diag", entry) => {
                if let Some((key, value)) = entry.split_once('=') {
                    status
                        .diagnostics
                        .push((key.to_string(), value.to_string()));
                }
            }
            _ => {}
        }
    }
    status
}

fn parse_phones(lines: &[String]) -> Vec<PhoneEntry> {
    lines
        .iter()
        .filter_map(|line| match split(line) {
            ("phone", entry) => {
                let (name, state) = entry.rsplit_once(' ')?;
                Some(PhoneEntry {
                    name: name.to_string(),
                    connected: state == "connected",
                })
            }
            _ => None,
        })
        .collect()
}

fn pairing_event(line: &str) -> Option<PhonePairingEvent> {
    match split(line) {
        ("uri", uri) => Some(PhonePairingEvent::Offer {
            uri: uri.to_string(),
            expires_in: Duration::from_secs(300),
        }),
        ("paired", name) => Some(PhonePairingEvent::Paired(name.to_string())),
        ("closed", _) => Some(PhonePairingEvent::Closed),
        ("error", message) => Some(PhonePairingEvent::Failed(message.to_string())),
        _ => None,
    }
}

impl NodeLink for ControlLink {
    fn notifications(&self) -> Option<edge_ui::NotificationSetup> {
        let reply = self.request("notification-setup").ok()?;
        let ("notifications", json) = split(reply.first()?) else {
            return None;
        };
        serde_json::from_str(json).ok()
    }

    fn install_notification_bridge(&self, shell: &std::path::Path) -> Result<String> {
        let setup = self
            .notifications()
            .context("Update and start edge-kvm before installing the bridge")?;
        crate::notification_setup::install_bridge(shell, &setup.socket_path)
    }

    fn status(&self) -> Option<NodeStatus> {
        self.request("status")
            .ok()
            .map(|lines| parse_status(&lines))
    }

    fn phones(&self) -> Option<Vec<PhoneEntry>> {
        self.request("list-phones")
            .ok()
            .map(|lines| parse_phones(&lines))
    }

    fn unpair_phone(&self, name: &str) -> Result<()> {
        let reply = self.request(&format!("unpair-phone {name}"))?;
        match reply.first().map(|line| split(line)) {
            Some(("unpaired" | "not-paired", _)) => Ok(()),
            Some(("error", message)) => anyhow::bail!("{message}"),
            _ => anyhow::bail!("unexpected reply from edge-kvm"),
        }
    }

    fn arm_pairing(&self) -> Result<()> {
        let reply = self.request("arm-pairing")?;
        match reply.first().map(|line| split(line)) {
            Some(("ok", _)) => Ok(()),
            Some(("error", message)) => anyhow::bail!("{message}"),
            _ => anyhow::bail!("unexpected reply from edge-kvm"),
        }
    }

    fn pair_phone(&self) -> Result<mpsc::Receiver<PhonePairingEvent>> {
        let stream = self.connect("pair-phone")?;
        let (events, received) = mpsc::channel();
        std::thread::spawn(move || {
            let mut expires_in = None;
            for line in BufReader::new(stream).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        events
                            .send(PhonePairingEvent::Failed(error.to_string()))
                            .ok();
                        return;
                    }
                };
                // `expires` follows `uri`; fold it into the offer.
                if let ("expires", seconds) = split(&line) {
                    expires_in = seconds.parse().ok().map(Duration::from_secs);
                    continue;
                }
                let Some(mut event) = pairing_event(&line) else {
                    continue;
                };
                if let PhonePairingEvent::Offer {
                    expires_in: offered,
                    ..
                } = &mut event
                    && let Some(seconds) = expires_in
                {
                    *offered = seconds;
                }
                if events.send(event).is_err() {
                    return;
                }
            }
        });
        Ok(received)
    }

    fn autostart(&self) -> Option<bool> {
        let output = Command::new("systemctl")
            .args(["--user", "is-enabled", SERVICE])
            .output()
            .ok()?;
        match String::from_utf8_lossy(&output.stdout).trim() {
            "enabled" => Some(true),
            "disabled" => Some(false),
            // Not installed as a service, so there's nothing to switch.
            _ => None,
        }
    }

    fn set_autostart(&self, enabled: bool) -> Result<()> {
        let status = Command::new("systemctl")
            .args([
                "--user",
                if enabled { "enable" } else { "disable" },
                SERVICE,
            ])
            .status()
            .context("failed to run systemctl")?;
        anyhow::ensure!(status.success(), "systemctl couldn't change {SERVICE}");
        Ok(())
    }

    fn open_log(&self) -> Result<()> {
        Command::new("xdg-open")
            .arg(&self.log_path)
            .spawn()
            .context("failed to run xdg-open")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn status_replies_parse_into_node_status() {
        let status = parse_status(&lines(
            "summary Main PC is controlling this computer · 4 ms\npeer Main PC\nping 4\ndiag last error=none\nend",
        ));
        assert_eq!(
            status.summary,
            "Main PC is controlling this computer · 4 ms"
        );
        assert!(status.connected);
        assert_eq!(status.peer.as_deref(), Some("Main PC"));
        assert_eq!(status.round_trip_ms, Some(4));
        assert_eq!(
            status.diagnostics,
            [("last error".to_string(), "none".to_string())]
        );
    }

    #[test]
    fn phone_names_may_contain_spaces() {
        assert_eq!(
            parse_phones(&lines(
                "phone CMF Phone 2 Pro connected\nphone Pixel disconnected\nend"
            )),
            [
                PhoneEntry {
                    name: "CMF Phone 2 Pro".to_string(),
                    connected: true,
                },
                PhoneEntry {
                    name: "Pixel".to_string(),
                    connected: false,
                },
            ]
        );
    }

    #[test]
    fn pairing_replies_become_events() {
        assert_eq!(
            pairing_event("paired Pixel"),
            Some(PhonePairingEvent::Paired("Pixel".to_string()))
        );
        assert_eq!(pairing_event("closed"), Some(PhonePairingEvent::Closed));
        assert_eq!(pairing_event("expires 300"), None);
    }
}
