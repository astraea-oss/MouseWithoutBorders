use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ksni::TrayMethods;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
pub enum TrayCommand {
    Quit,
}

#[derive(Clone)]
pub struct ReceiverTrayHandle {
    handle: ksni::Handle<ReceiverTray>,
    input_events: Arc<AtomicU64>,
    last_input_update: Arc<Mutex<Instant>>,
}

impl ReceiverTrayHandle {
    pub async fn spawn(
        listen: String,
        backend: String,
        allow_pairing: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TrayCommand>), ksni::Error> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let tray = ReceiverTray {
            state: "Starting".to_string(),
            listen,
            backend,
            allow_pairing,
            connected_peer: None,
            connections: 0,
            input_events: 0,
            clipboard_events: 0,
            last_error: None,
            command_tx,
        };
        let handle = tray.assume_sni_available(true).spawn().await?;
        Ok((
            Self {
                handle,
                input_events: Arc::new(AtomicU64::new(0)),
                last_input_update: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1))),
            },
            command_rx,
        ))
    }

    pub async fn listening(&self) {
        self.update(|tray| {
            tray.state = "Listening".to_string();
            tray.last_error = None;
        })
        .await;
    }

    pub async fn connected(&self, peer: String) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = "Connected".to_string();
            tray.connected_peer = Some(peer);
            tray.connections = tray.connections.saturating_add(1);
            tray.input_events = input_events;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn disconnected(&self, error: Option<String>) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.state = "Listening".to_string();
            tray.connected_peer = None;
            tray.input_events = input_events;
            tray.last_error = error;
        })
        .await;
    }

    pub async fn input_event(&self) {
        let total = self.input_events.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last_update = self.last_input_update.lock().await;
        if last_update.elapsed() < Duration::from_millis(750) {
            return;
        }
        *last_update = Instant::now();
        drop(last_update);

        self.update(move |tray| {
            tray.input_events = total;
            tray.last_error = None;
        })
        .await;
    }

    pub async fn clipboard_event(&self) {
        let input_events = self.input_events.load(Ordering::Relaxed);
        self.update(|tray| {
            tray.input_events = input_events;
            tray.clipboard_events = tray.clipboard_events.saturating_add(1);
            tray.last_error = None;
        })
        .await;
    }

    pub async fn error(&self, error: String) {
        self.update(|tray| {
            tray.last_error = Some(error);
        })
        .await;
    }

    pub async fn shutdown(&self) {
        self.handle.shutdown().await;
    }

    async fn update(&self, update: impl FnOnce(&mut ReceiverTray)) {
        let _ = self.handle.update(update).await;
    }
}

#[derive(Debug)]
pub struct ReceiverTray {
    state: String,
    listen: String,
    backend: String,
    allow_pairing: bool,
    connected_peer: Option<String>,
    connections: u64,
    input_events: u64,
    clipboard_events: u64,
    last_error: Option<String>,
    command_tx: mpsc::UnboundedSender<TrayCommand>,
}

impl ksni::Tray for ReceiverTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "edge-kvm-receiver".to_string()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }

    fn title(&self) -> String {
        format!("edge-kvm receiver: {}", self.state)
    }

    fn status(&self) -> ksni::Status {
        if self.last_error.is_some() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        mouse_icons(self.icon_color())
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: self.icon_pixmap(),
            title: "edge-kvm receiver".to_string(),
            description: self.description(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            disabled_item(format!("Status: {}", self.state)),
            disabled_item(format!("Listen: {}", self.listen)),
            disabled_item(format!("Input backend: {}", self.backend)),
            disabled_item(format!(
                "Pairing: {}",
                if self.allow_pairing {
                    "enabled"
                } else {
                    "disabled"
                }
            )),
            disabled_item(format!(
                "Peer: {}",
                self.connected_peer.as_deref().unwrap_or("none")
            )),
            disabled_item(format!("Connections: {}", self.connections)),
            disabled_item(format!("Input events: {}", self.input_events)),
            disabled_item(format!("Clipboard events: {}", self.clipboard_events)),
        ];

        if let Some(error) = &self.last_error {
            items.push(MenuItem::Separator);
            items.push(disabled_item(format!("Last error: {error}")));
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit receiver".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.command_tx.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

impl ReceiverTray {
    fn icon_color(&self) -> IconColor {
        if self.connected_peer.is_some() {
            IconColor::Connected
        } else if self.last_error.is_some() || self.state == "Listening" {
            IconColor::Disconnected
        } else {
            IconColor::Connecting
        }
    }

    fn description(&self) -> String {
        let mut lines = vec![
            format!("Status: {}", self.state),
            format!("Listen: {}", self.listen),
            format!("Input backend: {}", self.backend),
        ];
        if let Some(peer) = &self.connected_peer {
            lines.push(format!("Connected peer: {peer}"));
        }
        lines.push(format!("Connections: {}", self.connections));
        lines.push(format!("Input events: {}", self.input_events));
        lines.push(format!("Clipboard events: {}", self.clipboard_events));
        if let Some(error) = &self.last_error {
            lines.push(format!("Last error: {error}"));
        }
        lines.join("\n")
    }
}

#[derive(Clone, Copy)]
enum IconColor {
    Connecting,
    Connected,
    Disconnected,
}

fn mouse_icons(color: IconColor) -> Vec<ksni::Icon> {
    [22, 32]
        .into_iter()
        .map(|size| mouse_icon(size, color))
        .collect()
}

fn mouse_icon(size: i32, color: IconColor) -> ksni::Icon {
    let fill = match color {
        IconColor::Connecting => [0x9c, 0xa3, 0xaf],
        IconColor::Connected => [0x22, 0xc5, 0x5e],
        IconColor::Disconnected => [0xef, 0x44, 0x44],
    };
    let outline = [0x11, 0x18, 0x27];
    let highlight = [0xff, 0xff, 0xff];
    let mut data = vec![0; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let nx = (f64::from(x) + 0.5) / f64::from(size);
            let ny = (f64::from(y) + 0.5) / f64::from(size);
            let idx = ((y * size + x) * 4) as usize;

            let body = ellipse(nx, ny, 0.5, 0.56, 0.30, 0.39);
            let top = ellipse(nx, ny, 0.5, 0.30, 0.24, 0.20);
            let silhouette = body || top;
            if !silhouette {
                continue;
            }

            let border = !ellipse(nx, ny, 0.5, 0.56, 0.25, 0.34)
                || (top && !ellipse(nx, ny, 0.5, 0.30, 0.19, 0.15));
            let split = ny < 0.43 && (nx - 0.5).abs() < 0.018;
            let wheel = ellipse(nx, ny, 0.5, 0.34, 0.035, 0.075);
            let upper_highlight = ellipse(nx, ny, 0.41, 0.28, 0.055, 0.035);

            let (alpha, rgb) = if border || split {
                (0xee, outline)
            } else if wheel || upper_highlight {
                (0xd8, highlight)
            } else {
                (0xff, fill)
            };

            data[idx] = alpha;
            data[idx + 1] = rgb[0];
            data[idx + 2] = rgb[1];
            data[idx + 3] = rgb[2];
        }
    }

    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

fn ellipse(x: f64, y: f64, cx: f64, cy: f64, rx: f64, ry: f64) -> bool {
    let dx = (x - cx) / rx;
    let dy = (y - cy) / ry;
    dx * dx + dy * dy <= 1.0
}

fn disabled_item(label: String) -> ksni::MenuItem<ReceiverTray> {
    ksni::menu::StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}
