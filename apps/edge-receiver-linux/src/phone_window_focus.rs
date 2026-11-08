//! Small, opt-in Hyprland metadata feed. Polls at 4 Hz only while subscribed;
//! watch keeps only the newest sample, so a slow phone cannot build a backlog.
//! These are IPC layout bounds, not frame-synchronised capture coordinates.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use edge_protocol::{FocusMonitor, FocusRect, FocusedWindow, WindowFocusState};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::watch,
};

const MAX_REPLY: u64 = 1024 * 1024;
const SAMPLE_PERIOD: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub(crate) struct Sample {
    pub state: WindowFocusState,
    pub sampled_at_ms: u64,
}

pub(crate) struct Tracker {
    receiver: watch::Receiver<Option<Sample>>,
    _task: crate::AbortOnDropTask,
}

impl Tracker {
    pub fn start() -> Self {
        Self::start_at(socket_path())
    }

    fn start_at(path: Option<PathBuf>) -> Self {
        let (sender, receiver) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(SAMPLE_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut previous = None;
            loop {
                interval.tick().await;
                let state = match &path {
                    Some(path) => match sample(path).await {
                        Ok(state) => state,
                        Err(_) => WindowFocusState::Unavailable {
                            reason: "Hyprland window information is temporarily unavailable".into(),
                        },
                    },
                    None => WindowFocusState::Unavailable {
                        reason: "Receiver is not running inside a Hyprland session".into(),
                    },
                };
                if previous.as_ref() != Some(&state) {
                    previous = Some(state.clone());
                    let sampled_at_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    if sender
                        .send(Some(Sample {
                            state,
                            sampled_at_ms,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            receiver,
            _task: crate::AbortOnDropTask(task),
        }
    }
}

pub(crate) async fn changed(tracker: &mut Option<Tracker>) -> Option<Sample> {
    if let Some(tracker) = tracker
        && tracker.receiver.changed().await.is_ok()
    {
        return tracker.receiver.borrow_and_update().clone();
    }
    std::future::pending().await
}

pub(crate) fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(signature)
            .join(".socket.sock"),
    )
}

pub(crate) async fn request(path: &Path, command: &[u8]) -> Result<serde_json::Value> {
    tokio::time::timeout(Duration::from_millis(500), async {
        let mut socket = UnixStream::connect(path).await?;
        socket.write_all(command).await?;
        let mut reply = Vec::new();
        socket.take(MAX_REPLY + 1).read_to_end(&mut reply).await?;
        if reply.len() as u64 > MAX_REPLY {
            bail!("Hyprland reply too large");
        }
        Ok(serde_json::from_slice(&reply)?)
    })
    .await
    .context("Hyprland request timed out")?
}

pub(crate) async fn sample(path: &Path) -> Result<WindowFocusState> {
    let window = request(path, b"j/activewindow").await?;
    // Avoid querying monitor state if there is no focused window.
    if no_window(&window) {
        return Ok(WindowFocusState::NoWindow);
    }
    let monitors = request(path, b"j/monitors").await?;
    parse(window, monitors)
}

fn no_window(window: &serde_json::Value) -> bool {
    window.as_object().is_some_and(|o| o.is_empty())
        || window
            .get("address")
            .and_then(|v| v.as_str())
            .is_some_and(|a| a.is_empty() || a == "0x0")
}

#[derive(Deserialize)]
struct Workspace {
    id: i32,
}

#[derive(Deserialize)]
struct Window {
    address: String,
    at: [i32; 2],
    size: [u32; 2],
    class: String,
    title: String,
    workspace: Workspace,
    monitor: i32,
}

#[derive(Deserialize)]
struct Monitor {
    id: i32,
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    transform: u8,
}

fn bounded(text: String, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn intersection(a: &FocusRect, b: &FocusRect) -> Option<FocusRect> {
    let x = i64::from(a.x.max(b.x));
    let y = i64::from(a.y.max(b.y));
    let right = (i64::from(a.x) + i64::from(a.width)).min(i64::from(b.x) + i64::from(b.width));
    let bottom = (i64::from(a.y) + i64::from(a.height)).min(i64::from(b.y) + i64::from(b.height));
    (right > x && bottom > y).then(|| FocusRect {
        x: x as i32,
        y: y as i32,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    })
}

fn parse(window: serde_json::Value, monitors: serde_json::Value) -> Result<WindowFocusState> {
    if no_window(&window) {
        return Ok(WindowFocusState::NoWindow);
    }
    let window: Window = serde_json::from_value(window)?;
    let monitors: Vec<Monitor> = serde_json::from_value(monitors)?;
    let monitor = monitors
        .into_iter()
        .find(|m| m.id == window.monitor)
        .context("window monitor missing")?;
    if !monitor.scale.is_finite()
        || !(0.25..=8.0).contains(&monitor.scale)
        || monitor.transform > 7
        || monitor.width == 0
        || monitor.height == 0
        || window.size.contains(&0)
    {
        bail!("invalid window or monitor geometry");
    }
    let (width, height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    let logical_bounds = FocusRect {
        x: monitor.x,
        y: monitor.y,
        width: (f64::from(width) / monitor.scale).round() as u32,
        height: (f64::from(height) / monitor.scale).round() as u32,
    };
    let bounds = FocusRect {
        x: window.at[0],
        y: window.at[1],
        width: window.size[0],
        height: window.size[1],
    };
    let visible_bounds = intersection(&bounds, &logical_bounds);
    Ok(WindowFocusState::Focused {
        window: FocusedWindow {
            address: bounded(window.address, 128),
            app_id: bounded(window.class, 256),
            title: bounded(window.title, 1024),
            workspace_id: window.workspace.id,
            bounds,
            visible_bounds,
            monitor: FocusMonitor {
                id: monitor.id,
                name: bounded(monitor.name, 128),
                width_px: monitor.width,
                height_px: monitor.height,
                scale_milli: (monitor.scale * 1000.0).round() as u32,
                transform: monitor.transform,
                logical_bounds,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window(x: i32) -> serde_json::Value {
        json!({"address":"0x123", "at":[x,10], "size":[900,1000], "class":"browser", "title":"Example", "workspace":{"id":2}, "monitor":1})
    }

    #[test]
    fn scaled_rotated_monitor_and_partial_visibility() {
        let state = parse(window(-1000), json!([{"id":1,"name":"DP-1","x":-960,"y":0,"width":2560,"height":1440,"scale":1.5,"transform":3}])).unwrap();
        let WindowFocusState::Focused { window } = state else {
            panic!("expected window")
        };
        assert_eq!(
            window.monitor.logical_bounds,
            FocusRect {
                x: -960,
                y: 0,
                width: 960,
                height: 1707
            }
        );
        assert_eq!(
            window.visible_bounds,
            Some(FocusRect {
                x: -960,
                y: 10,
                width: 860,
                height: 1000
            })
        );
        assert_eq!(window.monitor.scale_milli, 1500);
    }

    #[test]
    fn no_focus_invalid_and_offscreen_geometry() {
        assert_eq!(
            parse(json!({}), json!([])).unwrap(),
            WindowFocusState::NoWindow
        );
        assert!(parse(window(0), json!([])).is_err());
        let monitor = json!([{"id":1,"name":"DP-1","x":0,"y":0,"width":3440,"height":1440,"scale":1,"transform":0}]);
        let WindowFocusState::Focused { window } = parse(window(-2000), monitor).unwrap() else {
            panic!("expected window")
        };
        assert_eq!(window.visible_bounds, None);
    }

    #[tokio::test]
    async fn ipc_sample_and_tracker_deliver_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hypr.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut command = [0; 64];
                let len = socket.read(&mut command).await.unwrap();
                assert_eq!(&command[..len], b"j/activewindow");
                socket.write_all(b"{}").await.unwrap();
            }
        });
        let _server = crate::AbortOnDropTask(server);
        let mut tracker = Some(Tracker::start_at(Some(path)));
        let first = tokio::time::timeout(Duration::from_secs(1), changed(&mut tracker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.state, WindowFocusState::NoWindow);
        assert!(
            tokio::time::timeout(Duration::from_millis(600), changed(&mut tracker))
                .await
                .is_err()
        );
        // Cancellation of changed() must not consume the next value.
        drop(_server);
        let next = tokio::time::timeout(Duration::from_secs(2), changed(&mut tracker))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(next.state, WindowFocusState::Unavailable { .. }));
    }
    #[tokio::test]
    #[ignore = "requires a running Hyprland session"]
    async fn live_hyprland_sample() {
        let state = sample(&socket_path().expect("Hyprland environment missing"))
            .await
            .unwrap();
        if let WindowFocusState::Focused { window } = state {
            assert!(window.bounds.width > 0 && window.bounds.height > 0);
            assert!(window.monitor.scale_milli > 0);
            println!(
                "Live focused-window bounds: {:?}; monitor: {:?}",
                window.bounds, window.monitor.logical_bounds
            );
        }
    }
}
