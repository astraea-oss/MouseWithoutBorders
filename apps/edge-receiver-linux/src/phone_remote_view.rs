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
    /// The phone's video decoding chain broke; restart it with a keyframe.
    Keyframe,
    /// Video: Media mode (true) or Text mode.
    Mode(bool),
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
        video: bool,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(8);
        let (sender, events) = mpsc::channel(2);
        let worker = Worker {
            view_id,
            control,
            clipboard,
            _owner: owner,
            control_state,
            limits: CaptureLimits::new(chunked, size),
            chunked,
        };
        let task = if video {
            tokio::spawn(run_video(worker, receiver, sender))
        } else {
            tokio::spawn(run(worker, receiver, sender, capture))
        };
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

    /// The phone lost its video decoding chain.
    pub fn keyframe(&self) {
        let _ = self.commands.try_send(CommandMessage::Keyframe);
    }

    /// Video: switch between Media mode (true) and Text mode.
    pub fn set_mode(&self, media: bool) {
        let _ = self.commands.try_send(CommandMessage::Mode(media));
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

pub(crate) fn same_target(a: &FocusedWindow, b: &FocusedWindow) -> bool {
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

/// Rolling numbers for the log: what a view costs per frame.
#[derive(Default)]
struct Stats {
    frames: u32,
    keyframes: u32,
    bytes: usize,
    capture: Duration,
    encode: Duration,
    acked: u32,
    ack: Duration,
    since: Option<Instant>,
}

impl Stats {
    fn report(&mut self, view_id: u64, video: bool) {
        let since = *self.since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed < Duration::from_secs(10) || self.frames == 0 {
            return;
        }
        let per = |d: Duration, n: u32| d.as_secs_f64() * 1000.0 / f64::from(n.max(1));
        tracing::info!(
            view_id,
            mode = if video { "hevc" } else { "jpeg" },
            fps = format!("{:.1}", f64::from(self.frames) / elapsed.as_secs_f64()),
            kb_per_s = (self.bytes as f64 / 1024.0 / elapsed.as_secs_f64()).round(),
            kb_per_frame = (self.bytes as f64 / 1024.0 / f64::from(self.frames)).round(),
            keyframes = self.keyframes,
            capture_ms = format!("{:.1}", per(self.capture, self.frames)),
            encode_ms = format!("{:.1}", per(self.encode, self.frames)),
            ack_ms = format!("{:.1}", per(self.ack, self.acked)),
            "remote view"
        );
        *self = Self {
            since: Some(Instant::now()),
            ..Self::default()
        };
    }
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
    let mut stats = Stats::default();
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pointer = None;
    // A drag's held left button, released if the view ends first.
    let mut held = HeldButton::default();
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
                Some(CommandMessage::Keyframe | CommandMessage::Mode(_)) => {}
                Some(CommandMessage::Ack(id)) => {
                    if let Some((pending, sent)) = waiting && pending == id {
                        stats.acked += 1;
                        stats.ack += sent.elapsed();
                        waiting = None;
                        stats.report(view_id, false);
                    }
                }
                Some(CommandMessage::Input(id, action)) => {
                    let result = match history.iter().find(|f| f.id == id) {
                        Some(frame) if control && control_state.as_ref().is_some_and(|s| !s.borrow().remote_input_busy) => apply_input(frame, action, clipboard.as_ref(), &mut pointer, &mut held).await,
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
                let started = Instant::now();
                let captured = capture_next(limits).await;
                stats.capture += started.elapsed();
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
                        stats.frames += 1;
                        stats.bytes += frame.jpeg.len();
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

/// The video worker. Frames come from [`crate::phone_remote_video::source`],
/// which captures only on damage; up to [`Mode::in_flight`] are sent ahead of
/// their acks so the round trip doesn't cap Media mode's frame rate.
///
/// [`Mode::in_flight`]: crate::phone_remote_video::Mode::in_flight
async fn run_video(
    worker: Worker,
    mut commands: mpsc::Receiver<CommandMessage>,
    events: mpsc::Sender<PhoneEvent>,
) {
    use crate::phone_remote_video::{Mode, SourceControl};
    let Worker {
        view_id,
        control,
        clipboard,
        _owner,
        control_state,
        chunked,
        ..
    } = worker;
    let source_control = std::sync::Arc::new(std::sync::Mutex::new(SourceControl::default()));
    let (frames_tx, mut frames) = mpsc::channel(1);
    let _source = crate::AbortOnDropTask(tokio::spawn(crate::phone_remote_video::source(
        source_control.clone(),
        frames_tx,
    )));
    let mut mode = Mode::Text;
    let mut in_flight: VecDeque<(u64, Instant)> = VecDeque::new();
    let mut watchdog = tokio::time::interval(Duration::from_millis(500));
    let mut stats = Stats::default();
    let mut pointer = None;
    // A drag's held left button, released if the view ends first.
    let mut held = HeldButton::default();
    let mut history: VecDeque<SentFrame> = VecDeque::new();
    let mut next_id = 0;
    let mut previous: Option<FocusedWindow> = None;
    let mut stable_since = Instant::now();
    let mut last_error = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(CommandMessage::Ack(id)) => {
                    while let Some(&(pending, sent)) = in_flight.front() {
                        if pending > id { break; }
                        in_flight.pop_front();
                        if pending == id { stats.acked += 1; stats.ack += sent.elapsed(); }
                    }
                    stats.report(view_id, true);
                }
                Some(CommandMessage::Input(id, action)) => {
                    let result = match history.iter().find(|f| f.id == id) {
                        Some(frame) if control && control_state.as_ref().is_some_and(|s| !s.borrow().remote_input_busy) => apply_input(frame, action, clipboard.as_ref(), &mut pointer, &mut held).await,
                        _ => Err(anyhow::anyhow!("That image is no longer current")),
                    };
                    if events.send(PhoneEvent::RemoteViewInputResult { view_id, error: result.err().map(|e| e.to_string()) }).await.is_err() { break; }
                }
                Some(CommandMessage::Keyframe) => source_control.lock().unwrap_or_else(|p| p.into_inner()).restart = true,
                Some(CommandMessage::Mode(media)) => {
                    mode = if media { Mode::Media } else { Mode::Text };
                    let mut control = source_control.lock().unwrap_or_else(|p| p.into_inner());
                    control.mode = mode;
                    control.restart = true;
                }
                // Video is captured at the window's native size.
                Some(CommandMessage::Size(..)) => {}
                None => break,
            },
            Some(encoded) = frames.recv(), if in_flight.len() < mode.in_flight() => match encoded {
                Ok(encoded) => {
                    let crate::phone_remote_video::Encoded { window, crop, data, keyframe, codec, width, height, capture, encode } = encoded;
                    if !previous.as_ref().is_some_and(|w| same_target(w, &window)) {
                        history.clear(); stable_since = Instant::now();
                    }
                    previous = Some(window.clone());
                    next_id += 1;
                    let interactive = control && stable_since.elapsed() >= Duration::from_secs(1);
                    let frame = RemoteViewFrame {
                        view_id, frame_id: next_id,
                        captured_at_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
                        window: window.clone(), crop: crop.clone(), width, height, interactive, jpeg: data,
                    };
                    stats.frames += 1;
                    stats.keyframes += u32::from(keyframe);
                    stats.bytes += frame.jpeg.len();
                    stats.capture += capture;
                    stats.encode += encode;
                    let (frame, chunks) = split_frame(frame, chunked);
                    // At 60 fps, taps on the displayed frame refer a few frames back.
                    history.push_back(SentFrame { id: next_id, window, crop, interactive, sent: Instant::now() });
                    while history.len() > 128 { history.pop_front(); }
                    if last_error.take().is_some() && events.send(PhoneEvent::RemoteViewStatus { view_id, active: true, control, error: None }).await.is_err() { break; }
                    let mut closed = false;
                    for chunk in chunks {
                        if events.send(chunk).await.is_err() { closed = true; break; }
                    }
                    if closed || events.send(PhoneEvent::RemoteVideoFrame { frame, keyframe, codec }).await.is_err() { break; }
                    in_flight.push_back((next_id, Instant::now()));
                }
                Err(error) => {
                    history.clear(); previous = None;
                    let reason = error.to_string();
                    if last_error.as_ref() != Some(&reason) {
                        last_error = Some(reason.clone());
                        if events.send(PhoneEvent::RemoteViewStatus { view_id, active: true, control, error: Some(reason) }).await.is_err() { break; }
                    }
                }
            },
            _ = watchdog.tick() => {
                if in_flight.front().is_some_and(|(_, sent)| sent.elapsed() > Duration::from_secs(4)) {
                    let _ = events.send(PhoneEvent::RemoteViewStatus { view_id, active: false, control: false, error: Some("Phone stopped displaying frames; open live view again".into()) }).await;
                    break;
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
    ensure_unlocked().await?;
    sample_window().await
}

/// Refuses a locked desktop, or one whose lock state is unknown.
pub(crate) async fn ensure_unlocked() -> Result<()> {
    let path = crate::phone_window_focus::socket_path().context("Hyprland session unavailable")?;
    let lock = crate::phone_window_focus::request(&path, b"j/locked").await?;
    ensure!(
        lock.get("locked").and_then(|v| v.as_bool()) == Some(false),
        "Desktop is locked or its lock state is unavailable"
    );
    Ok(())
}

/// The selected window, without the lock check.
pub(crate) async fn sample_window() -> Result<FocusedWindow> {
    let path = crate::phone_window_focus::socket_path().context("Hyprland session unavailable")?;
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
        RemoteAction::Tap { .. }
        | RemoteAction::Scroll { .. }
        | RemoteAction::DragStart { .. }
        | RemoteAction::DragMove { .. }
        | RemoteAction::DragEnd { .. }
        | RemoteAction::Type { .. } => {
            bail!("Pointer and typing actions use the Wayland input backend")
        }
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

/// Moves the virtual pointer to (`x`, `y`) of the frame's crop.
async fn move_pointer(
    frame: &SentFrame,
    x: u16,
    y: u16,
    pointer: &mut Option<edge_linux_input::HyprlandVirtualInputBackend>,
) -> Result<()> {
    let (x, y) = point(&frame.crop, x, y)?;
    let desktop = crate::phone_window_focus::desktop_bounds().await?;
    let dx = u32::try_from(x - i64::from(desktop.x))?;
    let dy = u32::try_from(y - i64::from(desktop.y))?;
    if pointer.is_none() {
        *pointer = Some(edge_linux_input::HyprlandVirtualInputBackend::connect()?);
    }
    pointer
        .as_ref()
        .unwrap()
        .move_absolute(dx, dy, desktop.width, desktop.height)
        .await?;
    Ok(())
}

async fn left_button(
    pointer: &Option<edge_linux_input::HyprlandVirtualInputBackend>,
    down: bool,
) -> Result<()> {
    let backend = pointer.as_ref().context("No pointer")?;
    backend
        .inject(edge_protocol::InputEvent::PointerButton {
            button: edge_protocol::MouseButton::Left,
            down,
        })
        .await?;
    Ok(())
}

/// The left button a drag holds. Also released on drop: stopping a view aborts
/// its task mid-loop, and the computer must never be left with it pressed.
#[derive(Default)]
struct HeldButton(Option<edge_linux_input::HyprlandVirtualInputBackend>);

impl HeldButton {
    fn is_held(&self) -> bool {
        self.0.is_some()
    }

    async fn release(&mut self) {
        if let Some(backend) = self.0.take() {
            let _ = backend.inject(left_up()).await;
        }
    }
}

impl Drop for HeldButton {
    fn drop(&mut self) {
        if let Some(backend) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = backend.inject(left_up()).await;
            });
        }
    }
}

fn left_up() -> edge_protocol::InputEvent {
    edge_protocol::InputEvent::PointerButton {
        button: edge_protocol::MouseButton::Left,
        down: false,
    }
}

const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTCTRL: u16 = 29;
const KEY_BACKSPACE: u16 = 14;
const KEY_V: u16 = 47;

/// The evdev code, and whether Shift is needed, for `c` on a US layout: the
/// virtual keyboard's own fixed keymap, so the desktop's layout doesn't matter.
fn us_key(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        _ => return None,
    })
}

/// Presses and releases `code`, holding each of `modifiers` around it. Every
/// press is released, even if an earlier event fails.
async fn press_key(
    backend: &edge_linux_input::HyprlandVirtualInputBackend,
    modifiers: &[u16],
    code: u16,
) -> Result<()> {
    use edge_protocol::InputEvent::Key;
    let mut results = Vec::new();
    for &modifier in modifiers {
        results.push(
            backend
                .inject(Key {
                    evdev_code: modifier,
                    down: true,
                })
                .await,
        );
    }
    results.push(
        backend
            .inject(Key {
                evdev_code: code,
                down: true,
            })
            .await,
    );
    results.push(
        backend
            .inject(Key {
                evdev_code: code,
                down: false,
            })
            .await,
    );
    for &modifier in modifiers.iter().rev() {
        results.push(
            backend
                .inject(Key {
                    evdev_code: modifier,
                    down: false,
                })
                .await,
        );
    }
    for result in results {
        result?;
    }
    Ok(())
}

/// Live typing: Backspaces, then `text` as key presses on the virtual
/// keyboard. Characters a US layout can't type (emoji, accents) go through the
/// clipboard and Ctrl+V, which needs the clipboard permission.
async fn type_text(
    backend: &edge_linux_input::HyprlandVirtualInputBackend,
    backspaces: u8,
    text: &str,
    clipboard: Option<&ClipboardConfig>,
) -> Result<()> {
    ensure!(text.len() <= 256, "Type at most 256 UTF-8 bytes at once");
    for _ in 0..backspaces {
        press_key(backend, &[], KEY_BACKSPACE).await?;
    }
    let mut pasted = String::new();
    let flush = async |pasted: &mut String| -> Result<()> {
        if pasted.is_empty() {
            return Ok(());
        }
        let config = clipboard.context("Typing this character needs the clipboard permission")?;
        edge_linux_input::write_clipboard_text(config, pasted).await?;
        pasted.clear();
        press_key(backend, &[KEY_LEFTCTRL], KEY_V).await
    };
    for c in text.chars() {
        match us_key(c) {
            Some((code, shift)) => {
                flush(&mut pasted).await?;
                let modifiers: &[u16] = if shift { &[KEY_LEFTSHIFT] } else { &[] };
                press_key(backend, modifiers, code).await?;
            }
            None => pasted.push(c),
        }
    }
    flush(&mut pasted).await
}

/// Scroll and drag (phone-window-gestures-v1), after `apply_input`'s checks.
/// Returns `None` for any other action.
async fn apply_gesture(
    frame: &SentFrame,
    action: &RemoteAction,
    pointer: &mut Option<edge_linux_input::HyprlandVirtualInputBackend>,
    held: &mut HeldButton,
) -> Option<Result<()>> {
    let result = match *action {
        RemoteAction::Scroll { x, y, dx, dy } => {
            async {
                ensure!(
                    (dx != 0 || dy != 0) && dx.abs() <= 10 && dy.abs() <= 10,
                    "Scroll at most 10 steps at a time"
                );
                move_pointer(frame, x, y, pointer).await?;
                // The backend's wheel axis is inverted: positive y scrolls up.
                pointer
                    .as_ref()
                    .unwrap()
                    .inject(edge_protocol::InputEvent::PointerWheel {
                        x: f64::from(dx),
                        y: -f64::from(dy),
                    })
                    .await?;
                Ok(())
            }
            .await
        }
        RemoteAction::DragStart { x, y } => {
            async {
                held.release().await;
                move_pointer(frame, x, y, pointer).await?;
                ensure!(
                    same_target(&frame.window, &focused().await?),
                    "Window changed; wait for a fresh image"
                );
                left_button(pointer, true).await?;
                held.0 = pointer.clone();
                Ok(())
            }
            .await
        }
        RemoteAction::DragMove { x, y } => {
            async {
                ensure!(held.is_held(), "No drag in progress");
                move_pointer(frame, x, y, pointer).await
            }
            .await
        }
        RemoteAction::DragEnd { x, y } => {
            async {
                ensure!(held.is_held(), "No drag in progress");
                let moved = move_pointer(frame, x, y, pointer).await;
                held.release().await;
                moved
            }
            .await
        }
        _ => return None,
    };
    if result.is_err() {
        // Never leave the button held after a failed step.
        held.release().await;
    }
    Some(result)
}

async fn apply_input(
    frame: &SentFrame,
    action: RemoteAction,
    clipboard: Option<&ClipboardConfig>,
    pointer: &mut Option<edge_linux_input::HyprlandVirtualInputBackend>,
    held: &mut HeldButton,
) -> Result<()> {
    if !frame.interactive || frame.sent.elapsed() >= Duration::from_secs(2) {
        held.release().await;
    }
    ensure!(frame.interactive, "Wait for the window to settle");
    ensure!(
        frame.sent.elapsed() < Duration::from_secs(2),
        "That image is too old; wait for a fresh frame"
    );
    let current = match focused().await {
        Ok(current) => current,
        Err(error) => {
            held.release().await;
            return Err(error);
        }
    };
    if !same_target(&frame.window, &current) {
        held.release().await;
        bail!("Selected window changed; wait for a fresh frame");
    }
    if let Some(result) = apply_gesture(frame, &action, pointer, held).await {
        return result;
    }
    if let RemoteAction::Type { backspaces, text } = &action {
        if pointer.is_none() {
            *pointer = Some(edge_linux_input::HyprlandVirtualInputBackend::connect()?);
        }
        return type_text(pointer.as_ref().unwrap(), *backspaces, text, clipboard).await;
    }
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
            apply_input(
                &frame,
                action.clone(),
                None,
                &mut None,
                &mut HeldButton::default()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("settle")
        );
        frame.interactive = true;
        frame.sent = Instant::now() - Duration::from_secs(3);
        assert!(
            apply_input(&frame, action, None, &mut None, &mut HeldButton::default())
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
    fn us_layout_covers_printable_ascii_and_nothing_else() {
        for c in (' '..='~').chain(['\n', '\t']) {
            assert!(us_key(c).is_some(), "{c:?}");
        }
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('Q'), Some((16, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('é'), None);
        assert_eq!(us_key('😀'), None);
        assert!(
            guarded_lua(
                &fixture(),
                &RemoteAction::Type {
                    backspaces: 1,
                    text: "x".into()
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn pointer_gestures_never_become_window_targeted_shortcuts() {
        let window = fixture();
        for action in [
            RemoteAction::Scroll {
                x: 1,
                y: 1,
                dx: 0,
                dy: 3,
            },
            RemoteAction::DragStart { x: 1, y: 1 },
            RemoteAction::DragMove { x: 2, y: 2 },
            RemoteAction::DragEnd { x: 3, y: 3 },
        ] {
            assert!(guarded_lua(&window, &action).is_err(), "{action:?}");
        }
        // Nothing held: releasing and dropping are no-ops.
        let mut held = HeldButton::default();
        assert!(!held.is_held());
        held.release().await;
        drop(held);
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
