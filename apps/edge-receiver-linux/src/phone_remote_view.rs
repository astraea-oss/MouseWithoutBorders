//! Prototype: two acknowledged, bounded JPEG crops per second. No listeners,
//! external input strings, persistent button holds, or unbounded frame queues.
use anyhow::{Context, Result, bail, ensure};
use edge_common::ClipboardConfig;
use edge_protocol::{
    FocusRect, FocusedWindow, PhoneEvent, REMOTE_VIEW_CHUNK_BYTES, REMOTE_VIEW_MAX_CHUNKED_EDGE,
    REMOTE_VIEW_MAX_CHUNKED_JPEG, REMOTE_VIEW_MAX_EDGE, REMOTE_VIEW_MAX_JPEG, RemoteAction,
    RemoteKey, RemoteViewFrame, WindowFocusState,
};
use std::{
    collections::VecDeque,
    io::Cursor,
    path::Path,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    sync::{OwnedMutexGuard, mpsc},
    time::Instant,
};

enum CommandMessage {
    Ack(u64),
    Input(u64, RemoteAction),
    Size(u16, u16),
}

/// What one capture may produce. Without chunks: the original single-message
/// budget. With chunks: up to the phone's on-screen size, never upscaled past
/// the window's native pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CaptureLimits {
    pub max_jpeg: usize,
    pub max_edge: u16,
    /// The phone's frame in device pixels, when it has said.
    pub target: Option<(u16, u16)>,
}

impl CaptureLimits {
    pub fn new(chunked: bool, target: Option<(u16, u16)>) -> Self {
        if chunked {
            Self {
                max_jpeg: REMOTE_VIEW_MAX_CHUNKED_JPEG,
                max_edge: REMOTE_VIEW_MAX_CHUNKED_EDGE,
                target: target.filter(|(w, h)| *w > 0 && *h > 0),
            }
        } else {
            Self {
                max_jpeg: REMOTE_VIEW_MAX_JPEG,
                max_edge: REMOTE_VIEW_MAX_EDGE,
                target: None,
            }
        }
    }

    /// Image pixels per logical desktop pixel for `crop` on a monitor with
    /// `native` (its output scale): enough to cover the phone's frame, as Fill
    /// does, and no more than the desktop actually has.
    fn scale(&self, crop: &FocusRect, native: f64) -> f64 {
        let (width, height) = (f64::from(crop.width), f64::from(crop.height));
        let fit_edge = f64::from(self.max_edge) / width.max(height);
        let wanted = match self.target {
            Some((w, h)) => (f64::from(w) / width).max(f64::from(h) / height),
            None => fit_edge,
        };
        wanted.min(fit_edge).min(native.max(1.0))
    }
}

pub(crate) struct RemoteView {
    pub view_id: u64,
    pub control: bool,
    commands: mpsc::Sender<CommandMessage>,
    events: mpsc::Receiver<PhoneEvent>,
    task: crate::AbortOnDropTask,
}

impl RemoteView {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        view_id: u64,
        control: bool,
        clipboard: Option<ClipboardConfig>,
        owner: Option<OwnedMutexGuard<()>>,
        control_state: Option<tokio::sync::watch::Receiver<crate::tray::TrayStatus>>,
        chunked: bool,
        size: Option<(u16, u16)>,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(8);
        let (sender, events) = mpsc::channel(2);
        let task = tokio::spawn(run(
            Worker {
                view_id,
                control,
                clipboard,
                _owner: owner,
                control_state,
                limits: CaptureLimits::new(chunked, size),
                chunked,
            },
            receiver,
            sender,
            capture,
        ));
        Self {
            view_id,
            control,
            commands,
            events,
            task: crate::AbortOnDropTask(task),
        }
    }

    pub fn ack(&self, frame_id: u64) {
        let _ = self.commands.try_send(CommandMessage::Ack(frame_id));
    }

    /// The phone's frame changed size, e.g. on rotation.
    pub fn resize(&self, width: u16, height: u16) {
        let _ = self.commands.try_send(CommandMessage::Size(width, height));
    }

    pub fn input(&self, frame_id: u64, action: RemoteAction) -> Result<()> {
        ensure!(self.control, "Enable touch control first");
        self.commands
            .try_send(CommandMessage::Input(frame_id, action))
            .context("Too many pending actions")
    }

    pub async fn stop(mut self) {
        self.task.0.abort();
        let _ = (&mut self.task.0).await;
    }
}

pub(crate) async fn changed(view: &mut Option<RemoteView>) -> Option<PhoneEvent> {
    if let Some(view) = view
        && let Some(event) = view.events.recv().await
    {
        return Some(event);
    }
    std::future::pending().await
}

struct SentFrame {
    id: u64,
    window: FocusedWindow,
    crop: FocusRect,
    interactive: bool,
    sent: Instant,
}

fn same_target(a: &FocusedWindow, b: &FocusedWindow) -> bool {
    a.address == b.address
        && a.bounds == b.bounds
        && a.visible_bounds == b.visible_bounds
        && a.workspace_id == b.workspace_id
        && a.monitor == b.monitor
}

type Captured = (FocusedWindow, FocusRect, Vec<u8>, u16, u16);

struct Worker {
    view_id: u64,
    control: bool,
    clipboard: Option<ClipboardConfig>,
    _owner: Option<OwnedMutexGuard<()>>,
    control_state: Option<tokio::sync::watch::Receiver<crate::tray::TrayStatus>>,
    limits: CaptureLimits,
    /// The phone reassembles `RemoteViewFrameChunk`s.
    chunked: bool,
}

async fn run<F, Fut>(
    worker: Worker,
    mut commands: mpsc::Receiver<CommandMessage>,
    events: mpsc::Sender<PhoneEvent>,
    mut capture_next: F,
) where
    F: FnMut(CaptureLimits) -> Fut + Send,
    Fut: std::future::Future<Output = Result<Captured>> + Send,
{
    let Worker {
        view_id,
        control,
        clipboard,
        _owner,
        control_state,
        mut limits,
        chunked,
    } = worker;
    let mut pace = tokio::time::interval(Duration::from_millis(500));
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pointer = None;
    let mut history: VecDeque<SentFrame> = VecDeque::new();
    let mut waiting: Option<(u64, Instant)> = None;
    let mut next_id = 0;
    let mut previous: Option<FocusedWindow> = None;
    let mut stable_since = Instant::now();
    let mut last_error = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(CommandMessage::Size(width, height)) => {
                    limits = CaptureLimits::new(chunked, Some((width, height)));
                }
                Some(CommandMessage::Ack(id)) => {
                    if waiting.is_some_and(|(pending, _)| pending == id) { waiting = None; }
                }
                Some(CommandMessage::Input(id, action)) => {
                    let result = match history.iter().find(|f| f.id == id) {
                        Some(frame) if control && control_state.as_ref().is_some_and(|s| !s.borrow().remote_input_busy) => apply_input(frame, action, clipboard.as_ref(), &mut pointer).await,
                        _ => Err(anyhow::anyhow!("That image is no longer current")),
                    };
                    if events.send(PhoneEvent::RemoteViewInputResult { view_id, error: result.err().map(|e| e.to_string()) }).await.is_err() { break; }
                }
                None => break,
            },
            _ = pace.tick() => {
                if let Some((_, sent)) = waiting {
                    if sent.elapsed() > Duration::from_secs(4) {
                        let _ = events.send(PhoneEvent::RemoteViewStatus { view_id, active: false, control: false, error: Some("Phone stopped displaying frames; open live view again".into()) }).await;
                        break;
                    }
                    continue;
                }
                let captured = capture_next(limits).await;
                match captured {
                    Ok((window, crop, jpeg, width, height)) => {
                        if !previous.as_ref().is_some_and(|w| same_target(w, &window)) {
                            history.clear(); stable_since = Instant::now();
                        }
                        previous = Some(window.clone());
                        next_id += 1;
                        let interactive = control && stable_since.elapsed() >= Duration::from_secs(1);
                        let frame = RemoteViewFrame {
                            view_id, frame_id: next_id,
                            captured_at_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
                            window: window.clone(), crop: crop.clone(), width, height, interactive, jpeg,
                        };
                        let (frame, chunks) = split_frame(frame, chunked);
                        history.push_back(SentFrame { id: next_id, window, crop, interactive, sent: Instant::now() });
                        while history.len() > 4 { history.pop_front(); }
                        if last_error.take().is_some() && events.send(PhoneEvent::RemoteViewStatus { view_id, active: true, control, error: None }).await.is_err() { break; }
                        let mut closed = false;
                        for chunk in chunks {
                            if events.send(chunk).await.is_err() { closed = true; break; }
                        }
                        if closed || events.send(PhoneEvent::RemoteViewFrame(frame)).await.is_err() { break; }
                        waiting = Some((next_id, Instant::now()));
                    }
                    Err(error) => {
                        history.clear(); previous = None;
                        let reason = error.to_string();
                        if last_error.as_ref() != Some(&reason) {
                            last_error = Some(reason.clone());
                            if events.send(PhoneEvent::RemoteViewStatus { view_id, active: true, control, error: Some(reason) }).await.is_err() { break; }
                        }
                    }
                }
            }
        }
    }
}

/// A frame too large for one message becomes ordered chunks, then the frame
/// itself with an empty `jpeg`. Small frames, and phones without chunk
/// support, keep the single-message form.
fn split_frame(mut frame: RemoteViewFrame, chunked: bool) -> (RemoteViewFrame, Vec<PhoneEvent>) {
    if !chunked || frame.jpeg.len() <= REMOTE_VIEW_MAX_JPEG {
        return (frame, Vec::new());
    }
    let jpeg = std::mem::take(&mut frame.jpeg);
    let total_bytes = jpeg.len() as u32;
    let chunks = jpeg
        .chunks(REMOTE_VIEW_CHUNK_BYTES)
        .enumerate()
        .map(|(index, bytes)| PhoneEvent::RemoteViewFrameChunk {
            view_id: frame.view_id,
            frame_id: frame.frame_id,
            offset: (index * REMOTE_VIEW_CHUNK_BYTES) as u32,
            total_bytes,
            bytes: bytes.to_vec(),
        })
        .collect();
    (frame, chunks)
}

async fn focused() -> Result<FocusedWindow> {
    let path = crate::phone_window_focus::socket_path().context("Hyprland session unavailable")?;
    let lock = crate::phone_window_focus::request(&path, b"j/locked").await?;
    ensure!(
        lock.get("locked").and_then(|v| v.as_bool()) == Some(false),
        "Desktop is locked or its lock state is unavailable"
    );
    match crate::phone_window_focus::sample(&path).await? {
        WindowFocusState::Focused { window } => Ok(window),
        _ => bail!("Select a visible window on the computer"),
    }
}

async fn capture(limits: CaptureLimits) -> Result<(FocusedWindow, FocusRect, Vec<u8>, u16, u16)> {
    let before = focused().await?;
    let crop = before
        .visible_bounds
        .clone()
        .context("Selected window is outside the visible monitor")?;
    let mut scale = limits.scale(&crop, f64::from(before.monitor.scale_milli) / 1000.0);
    for _ in 0..3 {
        let mut command = Command::new("grim");
        command
            .args([
                "-t",
                "jpeg",
                "-q",
                "50",
                "-s",
                &format!("{scale:.6}"),
                "-g",
                &format!("{},{} {}x{}", crop.x, crop.y, crop.width, crop.height),
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let mut child = command
                .spawn()
                .context("Install grim on the computer to enable live view")?;
            let mut jpeg = Vec::new();
            child
                .stdout
                .take()
                .unwrap()
                .take(limits.max_jpeg as u64 + 1)
                .read_to_end(&mut jpeg)
                .await?;
            if jpeg.len() > limits.max_jpeg {
                child.kill().await?;
                return Ok(None);
            }
            ensure!(child.wait().await?.success(), "Screen capture unavailable");
            Ok::<_, anyhow::Error>(Some(jpeg))
        })
        .await
        .context("Screen capture timed out")??;
        let Some(jpeg) = result else {
            scale *= 0.7;
            continue;
        };
        let (width, height) = image::ImageReader::new(Cursor::new(&jpeg))
            .with_guessed_format()?
            .into_dimensions()?;
        ensure!(
            width > 0 && height > 0 && width.max(height) <= u32::from(limits.max_edge),
            "Invalid captured dimensions"
        );
        let after = focused().await?;
        ensure!(
            same_target(&before, &after),
            "Window moved; waiting for a stable image"
        );
        return Ok((after, crop, jpeg, width as u16, height as u16));
    }
    bail!("Captured image is too large; reduce the window size")
}

fn point(crop: &FocusRect, x: u16, y: u16) -> Result<(i64, i64)> {
    ensure!(
        x <= 10000 && y <= 10000 && crop.width > 0 && crop.height > 0,
        "Tap is outside the displayed image"
    );
    Ok((
        i64::from(crop.x) + i64::from(x) * i64::from(crop.width - 1) / 10000,
        i64::from(crop.y) + i64::from(y) * i64::from(crop.height - 1) / 10000,
    ))
}

fn shortcut(key: RemoteKey) -> (&'static str, &'static str) {
    match key {
        RemoteKey::Escape => ("", "Escape"),
        RemoteKey::Tab => ("", "Tab"),
        RemoteKey::Enter => ("", "Return"),
        RemoteKey::Backspace => ("", "BackSpace"),
        RemoteKey::Delete => ("", "Delete"),
        RemoteKey::Left => ("", "Left"),
        RemoteKey::Right => ("", "Right"),
        RemoteKey::Up => ("", "Up"),
        RemoteKey::Down => ("", "Down"),
        RemoteKey::SelectAll => ("CTRL", "a"),
        RemoteKey::Copy => ("CTRL", "c"),
        RemoteKey::Paste => ("CTRL", "v"),
        RemoteKey::AddressBar => ("CTRL", "l"),
        // Delivered as real key presses in `apply_input`, never to one window.
        RemoteKey::SuperLeft
        | RemoteKey::SuperRight
        | RemoteKey::SuperUp
        | RemoteKey::SuperDown => ("SUPER", ""),
    }
}

const KEY_LEFTMETA: u16 = 125;

/// Presses Super+arrow on the virtual keyboard. Unlike `send_key_state` to a
/// window, this goes through the compositor, so the user's own Super+arrow
/// binds run (focus navigation by default). Every press is released, even if
/// an earlier event fails.
async fn press_super_arrow(
    backend: &edge_linux_input::HyprlandVirtualInputBackend,
    arrow: u16,
) -> Result<()> {
    use edge_protocol::InputEvent::Key;
    let meta_down = backend
        .inject(Key {
            evdev_code: KEY_LEFTMETA,
            down: true,
        })
        .await;
    let arrow_down = backend
        .inject(Key {
            evdev_code: arrow,
            down: true,
        })
        .await;
    let arrow_up = backend
        .inject(Key {
            evdev_code: arrow,
            down: false,
        })
        .await;
    let meta_up = backend
        .inject(Key {
            evdev_code: KEY_LEFTMETA,
            down: false,
        })
        .await;
    meta_down?;
    arrow_down?;
    arrow_up?;
    meta_up?;
    Ok(())
}

fn window_guard(window: &FocusedWindow) -> Result<String> {
    ensure!(
        window.address.starts_with("0x")
            && window.address.len() <= 32
            && window.address[2..].chars().all(|c| c.is_ascii_hexdigit()),
        "Invalid window identity"
    );
    let bounds = &window.bounds;
    Ok(format!(
        "local w=hl.get_active_window(); assert(w and w.address=='{}' and w.visible and w.accepts_input, 'Selected window changed'); local p,s=w.at,w.size; assert(p.x=={} and p.y=={} and s.x=={} and s.y=={}, 'Window moved'); ",
        window.address, bounds.x, bounds.y, bounds.width, bounds.height
    ))
}

fn guarded_lua(window: &FocusedWindow, action: &RemoteAction) -> Result<String> {
    let guard = window_guard(window)?;
    let (mods, key, target) = match action {
        RemoteAction::Tap { .. } => bail!("Pointer actions use the Wayland input backend"),
        RemoteAction::Key { key } if key.super_arrow().is_some() => {
            bail!("Super+arrow uses the Wayland input backend")
        }
        RemoteAction::Key { key } => {
            let (mods, key) = shortcut(*key);
            (mods, key, ",window=w")
        }
        RemoteAction::Paste { terminal, .. } => (
            if *terminal { "CTRL SHIFT" } else { "CTRL" },
            "v",
            ",window=w",
        ),
    };
    Ok(format!(
        "{guard}hl.dispatch(hl.dsp.send_key_state({{mods='{mods}',key='{key}',state='down'{target}}})); hl.dispatch(hl.dsp.send_key_state({{mods='{mods}',key='{key}',state='up'{target}}}))"
    ))
}

async fn eval(path: &Path, script: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_millis(700), async {
        let mut socket = UnixStream::connect(path).await?;
        socket
            .write_all(format!("eval {script}").as_bytes())
            .await?;
        let mut response = Vec::new();
        socket.take(8192).read_to_end(&mut response).await?;
        ensure!(
            String::from_utf8_lossy(&response).trim() == "ok",
            "Window changed or action was rejected by Hyprland"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Hyprland action timed out")?
}

async fn apply_input(
    frame: &SentFrame,
    action: RemoteAction,
    clipboard: Option<&ClipboardConfig>,
    pointer: &mut Option<edge_linux_input::HyprlandVirtualInputBackend>,
) -> Result<()> {
    ensure!(frame.interactive, "Wait for the window to settle");
    ensure!(
        frame.sent.elapsed() < Duration::from_secs(2),
        "That image is too old; wait for a fresh frame"
    );
    let current = focused().await?;
    ensure!(
        same_target(&frame.window, &current),
        "Selected window changed; wait for a fresh frame"
    );
    if let RemoteAction::Tap { x, y, right } = &action {
        let (x, y) = point(&frame.crop, *x, *y)?;
        let desktop = crate::phone_window_focus::desktop_bounds().await?;
        let dx = u32::try_from(x - i64::from(desktop.x))?;
        let dy = u32::try_from(y - i64::from(desktop.y))?;
        if pointer.is_none() {
            *pointer = Some(edge_linux_input::HyprlandVirtualInputBackend::connect()?);
        }
        let backend = pointer.as_ref().unwrap();
        backend
            .move_absolute(dx, dy, desktop.width, desktop.height)
            .await?;
        ensure!(
            frame.sent.elapsed() < Duration::from_secs(2)
                && same_target(&frame.window, &focused().await?),
            "Window changed; wait for a fresh image"
        );
        let button = if *right {
            edge_protocol::MouseButton::Right
        } else {
            edge_protocol::MouseButton::Left
        };
        let down = backend
            .inject(edge_protocol::InputEvent::PointerButton { button, down: true })
            .await;
        let up = backend
            .inject(edge_protocol::InputEvent::PointerButton {
                button,
                down: false,
            })
            .await;
        down?;
        up?;
        return Ok(());
    }
    if let RemoteAction::Key { key } = &action
        && let Some(arrow) = key.super_arrow()
    {
        if pointer.is_none() {
            *pointer = Some(edge_linux_input::HyprlandVirtualInputBackend::connect()?);
        }
        return press_super_arrow(pointer.as_ref().unwrap(), arrow).await;
    }
    if let RemoteAction::Paste { text, .. } = &action {
        ensure!(
            !text.is_empty() && text.len() <= 2048,
            "Paste at most 2048 UTF-8 bytes at once"
        );
        let config = clipboard.context("This phone does not have clipboard permission")?;
        edge_linux_input::write_clipboard_text(config, text).await?;
    }
    let script = guarded_lua(&frame.window, &action)?;
    eval(
        &crate::phone_window_focus::socket_path().context("Hyprland session unavailable")?,
        &script,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> FocusedWindow {
        let bounds = FocusRect {
            x: -900,
            y: 10,
            width: 800,
            height: 1000,
        };
        FocusedWindow {
            address: "0x123".into(),
            app_id: "test".into(),
            title: "Test".into(),
            workspace_id: 1,
            bounds: bounds.clone(),
            visible_bounds: Some(bounds),
            monitor: edge_protocol::FocusMonitor {
                id: 1,
                name: "DP-1".into(),
                width_px: 1440,
                height_px: 2560,
                scale_milli: 1000,
                transform: 0,
                logical_bounds: FocusRect {
                    x: -1440,
                    y: 0,
                    width: 1440,
                    height: 2560,
                },
            },
        }
    }

    #[tokio::test]
    async fn next_frame_waits_for_the_matching_display_acknowledgement() {
        let (commands, input) = mpsc::channel(8);
        let (output, mut events) = mpsc::channel(2);
        let task = tokio::spawn(run(
            Worker {
                view_id: 8,
                control: false,
                clipboard: None,
                _owner: None,
                control_state: None,
                limits: CaptureLimits::new(false, None),
                chunked: false,
            },
            input,
            output,
            |_| {
                let window = fixture();
                std::future::ready(Ok((
                    window.clone(),
                    window.bounds,
                    vec![0xff, 0xd8, 0xff, 0xd9],
                    640,
                    800,
                )))
            },
        ));
        let _task = crate::AbortOnDropTask(task);
        assert!(matches!(
            events.recv().await,
            Some(PhoneEvent::RemoteViewFrame(RemoteViewFrame {
                frame_id: 1,
                ..
            }))
        ));
        commands.send(CommandMessage::Ack(99)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(650), events.recv())
                .await
                .is_err()
        );
        commands.send(CommandMessage::Ack(1)).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(PhoneEvent::RemoteViewFrame(RemoteViewFrame {
                frame_id: 2,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn old_and_unsettled_frames_are_rejected_before_injection() {
        let mut frame = SentFrame {
            id: 1,
            window: fixture(),
            crop: fixture().bounds,
            interactive: false,
            sent: Instant::now(),
        };
        let action = RemoteAction::Key {
            key: RemoteKey::Enter,
        };
        assert!(
            apply_input(&frame, action.clone(), None, &mut None)
                .await
                .unwrap_err()
                .to_string()
                .contains("settle")
        );
        frame.interactive = true;
        frame.sent = Instant::now() - Duration::from_secs(3);
        assert!(
            apply_input(&frame, action, None, &mut None)
                .await
                .unwrap_err()
                .to_string()
                .contains("old")
        );
    }

    #[test]
    fn generated_actions_are_targeted_and_never_interpolate_phone_text() {
        let mut window = fixture();
        let action = RemoteAction::Paste {
            text: "'); os.execute('bad'); --".into(),
            terminal: false,
        };
        let script = guarded_lua(&window, &action).unwrap();
        assert!(!script.contains("os.execute"));
        assert!(script.contains("window=w") && script.contains("w.address=='0x123'"));
        window.address = "0x123';bad()".into();
        assert!(guarded_lua(&window, &action).is_err());
        let mut moved = fixture();
        moved.bounds.x += 1;
        assert!(!same_target(&fixture(), &moved));
    }

    #[test]
    fn capture_scale_covers_the_phone_frame_without_upscaling_the_desktop() {
        let crop = |width, height| FocusRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        // Old phones: the 960 px single-message budget, never above native.
        let old = CaptureLimits::new(false, Some((1080, 1830)));
        assert_eq!(old.target, None);
        assert!((old.scale(&crop(1920, 1080), 1.0) - 0.5).abs() < 1e-9);
        assert_eq!(old.scale(&crop(400, 300), 1.0), 1.0);
        let phone = CaptureLimits::new(true, Some((1080, 1830)));
        // A tall window is shown at native size: Fill would enlarge it.
        assert_eq!(phone.scale(&crop(875, 1428), 1.0), 1.0);
        // HiDPI monitors give their physical pixels.
        assert_eq!(phone.scale(&crop(875, 1428), 2.0), 1830.0 / 1428.0);
        // A huge window only needs enough to cover the frame.
        assert!((phone.scale(&crop(3440, 4000), 1.0) - 1830.0 / 4000.0).abs() < 1e-9);
        // The edge cap still applies.
        let wide = CaptureLimits::new(true, Some((4000, 4000)));
        assert!(wide.scale(&crop(8000, 1000), 4.0) * 8000.0 <= 4096.0 + 1e-6);
    }

    #[test]
    fn large_frames_split_into_ordered_chunks_only_for_chunk_phones() {
        let frame = |len| RemoteViewFrame {
            view_id: 2,
            frame_id: 5,
            captured_at_ms: 0,
            window: fixture(),
            crop: fixture().bounds,
            width: 10,
            height: 10,
            interactive: false,
            jpeg: vec![7; len],
        };
        let (whole, chunks) = split_frame(frame(REMOTE_VIEW_MAX_JPEG), true);
        assert!(chunks.is_empty() && whole.jpeg.len() == REMOTE_VIEW_MAX_JPEG);
        let (whole, chunks) = split_frame(frame(200_000), false);
        assert!(chunks.is_empty() && whole.jpeg.len() == 200_000);
        let (last, chunks) = split_frame(frame(200_000), true);
        assert!(last.jpeg.is_empty());
        assert_eq!(
            chunks.len(),
            200_000_usize.div_ceil(REMOTE_VIEW_CHUNK_BYTES)
        );
        let mut joined = Vec::new();
        for chunk in chunks {
            let PhoneEvent::RemoteViewFrameChunk {
                view_id: 2,
                frame_id: 5,
                offset,
                total_bytes: 200_000,
                bytes,
            } = chunk
            else {
                panic!("unexpected chunk {chunk:?}");
            };
            assert_eq!(offset as usize, joined.len());
            assert!(bytes.len() <= REMOTE_VIEW_CHUNK_BYTES);
            joined.extend(bytes);
        }
        assert_eq!(joined, vec![7; 200_000]);
    }

    #[test]
    fn super_arrows_never_become_window_targeted_shortcuts() {
        let window = fixture();
        for (key, code) in [
            (RemoteKey::SuperLeft, 105),
            (RemoteKey::SuperRight, 106),
            (RemoteKey::SuperUp, 103),
            (RemoteKey::SuperDown, 108),
        ] {
            assert_eq!(key.super_arrow(), Some(code));
            // A window-targeted send_key_state would bypass the user's binds.
            assert!(guarded_lua(&window, &RemoteAction::Key { key }).is_err());
        }
        assert_eq!(RemoteKey::Left.super_arrow(), None);
        assert!(
            guarded_lua(
                &window,
                &RemoteAction::Key {
                    key: RemoteKey::Left
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn maps_edges_and_negative_origins_without_leaving_crop() {
        let crop = FocusRect {
            x: -960,
            y: 10,
            width: 960,
            height: 1707,
        };
        assert_eq!(point(&crop, 0, 0).unwrap(), (-960, 10));
        assert_eq!(point(&crop, 10000, 10000).unwrap(), (-1, 1716));
        assert!(point(&crop, 10001, 0).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a running Hyprland session and grim"]
    async fn live_cropped_capture() {
        let (_, crop, jpeg, width, height) =
            capture(CaptureLimits::new(false, None)).await.unwrap();
        assert!(jpeg.len() <= REMOTE_VIEW_MAX_JPEG);
        assert!(width.max(height) <= REMOTE_VIEW_MAX_EDGE);
        println!(
            "Captured {}x{} JPEG ({} bytes), crop {}x{}",
            width,
            height,
            jpeg.len(),
            crop.width,
            crop.height
        );
    }
}
