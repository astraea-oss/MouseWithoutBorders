//! Settings integration for the existing Quickshell centre. No notification UI.
use crate::notification_hub::NotificationHub;
use anyhow::{Context, Result, ensure};
use edge_ui::{NotificationComputer, NotificationSetup};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const BRIDGE: &str = include_str!("../../../integrations/quickshell/EdgeNotificationBridge.qml");

#[derive(Clone)]
pub(crate) struct NotificationControl {
    pub identity: String,
    pub coordinator: String,
    pub configured_peer: Option<NotificationComputer>,
    pub hub: Option<NotificationHub>,
    pub error: Option<String>,
    pub connected: std::sync::Arc<std::sync::Mutex<Option<NotificationComputer>>>,
}

impl NotificationControl {
    pub async fn setup(&self) -> Result<NotificationSetup> {
        let state = edge_common::default_state_dir();
        let pins = edge_crypto::PinStore::load_or_default(state.join("pins.toml")).await?;
        let mut computers: Vec<_> = pins
            .peers
            .into_iter()
            .map(|(name, pin)| NotificationComputer {
                name,
                fingerprint: pin.fingerprint,
            })
            .collect();
        if let Some(peer) = &self.configured_peer
            && !computers.iter().any(|p| p.fingerprint == peer.fingerprint)
        {
            computers.push(peer.clone());
        }
        if let Some(peer) = self.connected.lock().ok().and_then(|p| p.clone()) {
            prefer_connected_name(&mut computers, peer);
        }
        let shell_path = detect_shell();
        let socket_path = state.join("notifications/bridge.sock");
        let bridge_installed = shell_path
            .as_deref()
            .is_some_and(|shell| integration_installed(shell, &socket_path));
        Ok(NotificationSetup {
            identity: self.identity.clone(),
            computers,
            status: match (&self.hub, &self.error) {
                (_, Some(error)) => format!("Failed: {error}"),
                (Some(hub), _) => hub.status().await,
                _ => "Off".into(),
            },
            active: self.hub.is_some(),
            has_store: match &self.hub {
                Some(hub) => hub.coordinator_locked().await,
                None => {
                    crate::notification_hub::coordinator_locked_on_disk(
                        &state.join("notifications/store.json"),
                    )
                    .await?
                }
            },
            coordinator: self.coordinator.clone(),
            shell_path,
            socket_path,
            bridge_installed,
            bridge_connected: self
                .hub
                .as_ref()
                .is_some_and(NotificationHub::bridge_connected),
        })
    }
}

fn prefer_connected_name(computers: &mut Vec<NotificationComputer>, peer: NotificationComputer) {
    computers.retain(|p| p.fingerprint != peer.fingerprint);
    computers.insert(0, peer);
}

pub(crate) fn detect_shell() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    let root = config.join("quickshell");
    let mut paths = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        if !matches!(
            exe.file_name().and_then(|s| s.to_str()),
            Some("qs" | "quickshell")
        ) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<_> = cmdline
            .split(|b| *b == 0)
            .filter_map(|a| std::str::from_utf8(a).ok())
            .collect();
        let mut path = root.join("shell.qml");
        for pair in args.windows(2) {
            match pair[0] {
                "-c" | "--config" => path = root.join(pair[1]).join("shell.qml"),
                "-p" | "--path" => {
                    path = PathBuf::from(pair[1]);
                    if path.is_relative() {
                        let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
                            continue;
                        };
                        path = cwd.join(path);
                    }
                    if path.is_dir() {
                        path = path.join("shell.qml");
                    }
                }
                _ => {}
            }
        }
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    if paths.len() == 1 {
        paths.pop()
    } else if paths.is_empty() && root.join("shell.qml").is_file() {
        Some(root.join("shell.qml"))
    } else {
        None
    }
}

pub(crate) fn integration_installed(path: &Path, socket: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|original| {
        original.contains("    EdgeNotificationBridge {")
            && patch_shell(&original, socket).is_ok_and(|patched| patched == original)
    }) && path.parent().is_some_and(|p| {
        std::fs::read_to_string(p.join("EdgeNotificationBridge.qml")).is_ok_and(|s| s == BRIDGE)
    })
}

fn replace_once(text: &str, before: &str, after: &str) -> Result<String> {
    ensure!(
        text.matches(before).count() == 1,
        "This shell layout is not supported automatically ({before}). No shell files were changed."
    );
    Ok(text.replacen(before, after, 1))
}

fn patch_shell(original: &str, socket: &Path) -> Result<String> {
    let socket = serde_json::to_string(socket.to_str().context("socket path must be UTF-8")?)?;
    let bridge = format!(
        r#"    EdgeNotificationBridge {{
        server: notificationServer
        state: notificationState
        timestamps: root.notificationTimes
        socketPath: {socket}
        onEntryClosed: id => {{
            root.removeNotificationPopup(id);
            if (root.notificationTimes[id] === undefined) return;
            let times = Object.assign({{}}, root.notificationTimes);
            delete times[id];
            notificationState.times = JSON.stringify(times);
        }}
    }}"#
    );
    if let Some(start) = original.find("    EdgeNotificationBridge {") {
        ensure!(
            original.matches("    EdgeNotificationBridge {").count() == 1,
            "Multiple bridges found; shell left unchanged"
        );
        let end = start
            + original[start..]
                .find("\n    }\n")
                .context("Unrecognized existing bridge; shell left unchanged")?
            + "\n    }".len();
        return Ok(format!(
            "{}{}{}",
            &original[..start],
            bridge,
            &original[end..]
        ));
    }
    // Deliberately target the reviewed Tera/Ember centre. Refuse older/unknown
    // layouts instead of silently changing notification retention or DND.
    for marker in [
        "id: notificationState",
        "id: notificationServer",
        "function retainNotification",
        "function releaseTransients",
        "function removeNotificationPopup",
        "notification.lastGeneration",
        "notification.transient",
    ] {
        ensure!(
            original.contains(marker),
            "Update this Quickshell notification centre first: missing {marker}. No shell files were changed."
        );
    }
    let mut text = replace_once(
        original,
        "property string times: \"{}\"",
        "property string times: \"{}\"\n        property string epoch: \"\"\n        property string offlineClosures: \"[]\"",
    )?;
    text = replace_once(
        &text,
        "root.retainNotification(notification, Date.now());",
        "root.retainNotification(notification, Number(notification.hints[\"x-edge-kvm-posted-ms\"]) || Date.now());",
    )?;
    text = replace_once(
        &text,
        "if (root.dnd) return;",
        "if (root.dnd || notification.hints[\"x-edge-kvm-replay\"]) return;",
    )?;
    replace_once(
        &text,
        "    component EmberButton:",
        &format!("{bridge}\n\n    component EmberButton:"),
    )
}

pub(crate) fn install_bridge(shell: &Path, socket: &Path) -> Result<String> {
    ensure!(
        socket.is_absolute(),
        "Notification socket path must be absolute"
    );
    ensure!(shell.is_file(), "Choose an existing Quickshell shell.qml");
    // Preserve the launch path: QML resolves local components beside this URL,
    // even when shell.qml itself is a symlink into a dotfiles checkout.
    let shell = if shell.is_absolute() {
        shell.to_path_buf()
    } else {
        std::env::current_dir()?.join(shell)
    };
    ensure!(
        shell.file_name().is_some_and(|n| n == "shell.qml"),
        "Choose shell.qml"
    );
    let parent = shell.parent().context("shell has no directory")?;
    let original = std::fs::read_to_string(&shell)?;
    let patched = patch_shell(&original, socket)?;
    if original == patched
        && std::fs::read_to_string(parent.join("EdgeNotificationBridge.qml"))
            .is_ok_and(|text| text == BRIDGE)
    {
        // Re-request a reload even when the files already match: a previous
        // symlink/atomic replacement may never have reached the running shell.
        // A directory event also recovers a watch detached by an earlier
        // atomic shell replacement. The component contents are identical here.
        let refresh = parent.join(format!(
            ".edge-bridge-reload-{}.qml",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::write(&refresh, BRIDGE)?;
        std::fs::rename(refresh, parent.join("EdgeNotificationBridge.qml"))?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&shell)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        return Ok("Requested Quickshell bridge reload; check connection status".into());
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let backup = parent.join(format!("shell.qml.before-edge-notifications-{stamp}"));
    // Validate all edits before touching the actual shell. Preserve modes and
    // back up both files; reinstalling never overwrites a previous backup.
    std::fs::copy(&shell, &backup)?;
    let component = parent.join("EdgeNotificationBridge.qml");
    if component.exists() {
        std::fs::copy(
            &component,
            parent.join(format!("EdgeNotificationBridge.qml.before-{stamp}")),
        )?;
    }
    let component_tmp = parent.join(format!(".edge-bridge-{stamp}.qml"));
    std::fs::write(&component_tmp, BRIDGE)?;
    std::fs::rename(&component_tmp, &component)?;
    // Keep the watched shell inode, including when shell.qml is a symlink into
    // a dotfiles checkout. Replacing its target can detach Quickshell's watch.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).open(&shell)?;
    file.write_all(patched.as_bytes())?;
    file.set_len(patched.len() as u64)?;
    file.sync_all()?;
    Ok(format!(
        "Bridge installed. Quickshell reloads automatically; backup: {}",
        backup.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const SHELL: &str = "id: notificationState\nid: notificationServer\nfunction retainNotification\nfunction releaseTransients\nfunction removeNotificationPopup\nnotification.lastGeneration\nnotification.transient\nproperty string times: \"{}\"\nroot.retainNotification(notification, Date.now());\nif (root.dnd) return;\n    component EmberButton: Rectangle {}\n";
    #[test]
    fn authenticated_name_replaces_a_legacy_peer_label_for_the_same_key() {
        let mut computers = vec![NotificationComputer {
            name: "Peer".into(),
            fingerprint: "lua-key".into(),
        }];
        prefer_connected_name(
            &mut computers,
            NotificationComputer {
                name: "Lua".into(),
                fingerprint: "lua-key".into(),
            },
        );
        assert_eq!(computers.len(), 1);
        assert_eq!(computers[0].name, "Lua");
    }

    #[test]
    fn installing_and_reinstalling_keeps_one_bridge_and_silent_replay() {
        let once = patch_shell(SHELL, Path::new("/tmp/a.sock")).unwrap();
        let twice = patch_shell(&once, Path::new("/tmp/b.sock")).unwrap();
        assert_eq!(twice.matches("    EdgeNotificationBridge {").count(), 1);
        assert_eq!(twice.matches("property string epoch").count(), 1);
        assert!(twice.contains("x-edge-kvm-replay"));
        assert!(twice.contains("x-edge-kvm-posted-ms"));
        assert!(twice.contains("/tmp/b.sock"));
        assert!(!twice.contains("/tmp/a.sock"));
    }
    #[test]
    #[ignore = "requires EDGE_KVM_SHELL_FIXTURE and qmlformat"]
    fn reviewed_shell_installation_parses_and_preserves_a_backup() {
        let fixture = std::env::var_os("EDGE_KVM_SHELL_FIXTURE").expect("set shell fixture path");
        let original = std::fs::read_to_string(fixture).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("shell.qml");
        std::fs::write(&shell, &original).unwrap();
        install_bridge(&shell, Path::new("/tmp/edge-test.sock")).unwrap();
        let output = std::process::Command::new("qmlformat")
            .arg("-n")
            .arg(&shell)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(integration_installed(
            &shell,
            Path::new("/tmp/edge-test.sock")
        ));
        let backup = std::fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("shell.qml.before-")
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(backup.path()).unwrap(), original);
        let count = std::fs::read_dir(temp.path()).unwrap().count();
        install_bridge(&shell, Path::new("/tmp/edge-test.sock")).unwrap();
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), count);
    }

    #[test]
    fn unsupported_shell_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.qml");
        std::fs::write(&path, "ShellRoot {}").unwrap();
        assert!(install_bridge(&path, Path::new("/tmp/edge-test.sock")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ShellRoot {}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
