use std::sync::mpsc;

use edge_geometry::Size;
use edge_keymap::{WindowsScanCode, windows_scancode_to_evdev};
use edge_protocol::{ControlEvent, Edge, InputEvent};

#[derive(Debug, thiserror::Error)]
pub enum WindowsInputError {
    #[error("Windows input capture is only available on Windows")]
    UnsupportedPlatform,
    #[error("unmapped Windows scan code {scan_code:#x}, extended={extended}")]
    UnmappedKey { scan_code: u16, extended: bool },
    #[error("Windows tray error: {0}")]
    Tray(String),
    #[error("Windows input capture is already running")]
    CaptureAlreadyRunning,
    #[error("Windows input capture error: {0}")]
    Capture(String),
    #[error("Windows clipboard error: {0}")]
    Clipboard(String),
}

pub type Result<T> = std::result::Result<T, WindowsInputError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    LocalActive,
    RemoteActive,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    pub edge: Edge,
    pub remote_size: Size,
}

#[derive(Debug, Clone)]
pub enum CapturedInput {
    Input(InputEvent),
    Control(ControlEvent),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStatsSnapshot {
    pub active: bool,
    pub mouse_hook_events: u64,
    pub keyboard_hook_events: u64,
    pub input_events: u64,
    pub control_events: u64,
    pub enter_events: u64,
    pub release_events: u64,
    pub return_edge_hits: u64,
    pub send_failures: u64,
    pub unmapped_keys: u64,
}

pub fn map_key(scan_code: u16, extended: bool) -> Result<u16> {
    windows_scancode_to_evdev(WindowsScanCode {
        scan_code,
        extended,
    })
    .ok_or(WindowsInputError::UnmappedKey {
        scan_code,
        extended,
    })
}

#[cfg(windows)]
pub fn start_capture(config: CaptureConfig) -> Result<mpsc::Receiver<CapturedInput>> {
    capture::start(config)
}

#[cfg(not(windows))]
pub fn start_capture(_config: CaptureConfig) -> Result<mpsc::Receiver<CapturedInput>> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn install_hooks() -> Result<()> {
    tracing::info!("Windows hook installation placeholder");
    Ok(())
}

#[cfg(not(windows))]
pub fn install_hooks() -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn run_tray(status: &str) -> Result<()> {
    tray::run(status)
}

#[cfg(not(windows))]
pub fn run_tray(_status: &str) -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn force_release_to_local() {
    capture::force_release_to_local()
}

#[cfg(not(windows))]
pub fn force_release_to_local() {}

#[cfg(windows)]
pub fn capture_stats() -> CaptureStatsSnapshot {
    capture::stats_snapshot()
}

#[cfg(not(windows))]
pub fn capture_stats() -> CaptureStatsSnapshot {
    CaptureStatsSnapshot::default()
}

#[cfg(windows)]
pub fn read_clipboard_text(max_bytes: usize) -> Result<Option<String>> {
    clipboard::read_text(max_bytes)
}

#[cfg(not(windows))]
pub fn read_clipboard_text(_max_bytes: usize) -> Result<Option<String>> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn write_clipboard_text(text: &str, max_bytes: usize) -> Result<()> {
    clipboard::write_text(text, max_bytes)
}

#[cfg(not(windows))]
pub fn write_clipboard_text(_text: &str, _max_bytes: usize) -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
mod clipboard {
    use std::{ptr::null_mut, slice};

    use windows_sys::Win32::{
        Foundation::{GetLastError, GlobalFree},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        },
    };

    use crate::{Result, WindowsInputError};

    const CF_UNICODETEXT: u32 = 13;

    pub fn read_text(max_bytes: usize) -> Result<Option<String>> {
        let _clipboard = OpenClipboardGuard::open()?;
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle.is_null() {
            return Ok(None);
        }

        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            return Err(last_error("GlobalLock"));
        }
        let _lock = GlobalLockGuard { handle };

        let words = unsafe { slice::from_raw_parts(ptr.cast::<u16>(), GlobalSize(handle) / 2) };
        let end = words
            .iter()
            .position(|word| *word == 0)
            .unwrap_or(words.len());
        let text = String::from_utf16_lossy(&words[..end]);
        if text.len() > max_bytes {
            return Err(WindowsInputError::Clipboard(format!(
                "clipboard text exceeds configured max_bytes ({max_bytes})"
            )));
        }

        Ok(Some(text))
    }

    pub fn write_text(text: &str, max_bytes: usize) -> Result<()> {
        if text.len() > max_bytes {
            return Err(WindowsInputError::Clipboard(format!(
                "clipboard text exceeds configured max_bytes ({max_bytes})"
            )));
        }

        let _clipboard = OpenClipboardGuard::open()?;
        if unsafe { EmptyClipboard() } == 0 {
            return Err(last_error("EmptyClipboard"));
        }

        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * std::mem::size_of::<u16>();
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            return Err(last_error("GlobalAlloc"));
        }

        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            unsafe {
                GlobalFree(handle);
            }
            return Err(last_error("GlobalLock"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr.cast::<u16>(), wide.len());
            GlobalUnlock(handle);
        }

        if unsafe { SetClipboardData(CF_UNICODETEXT, handle) }.is_null() {
            unsafe {
                GlobalFree(handle);
            }
            return Err(last_error("SetClipboardData"));
        }

        Ok(())
    }

    struct OpenClipboardGuard;

    impl OpenClipboardGuard {
        fn open() -> Result<Self> {
            if unsafe { OpenClipboard(null_mut()) } == 0 {
                return Err(last_error("OpenClipboard"));
            }
            Ok(Self)
        }
    }

    impl Drop for OpenClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    struct GlobalLockGuard {
        handle: *mut std::ffi::c_void,
    }

    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            unsafe {
                GlobalUnlock(self.handle);
            }
        }
    }

    fn last_error(operation: &str) -> WindowsInputError {
        WindowsInputError::Clipboard(format!("{operation} failed with Win32 error {}", unsafe {
            GetLastError()
        }))
    }
}

#[cfg(windows)]
mod capture {
    use std::{
        ptr::null_mut,
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
    };

    use edge_protocol::Edge;
    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CallNextHookEx, CreateCursor, DestroyCursor, DispatchMessageW, GetMessageW,
            GetSystemMetrics, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLMHF_INJECTED, MSG,
            MSLLHOOKSTRUCT, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO,
            OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP,
            OCR_WAIT, SPI_SETCURSORS, SetCursorPos, SetSystemCursor, SetWindowsHookExW, ShowCursor,
            SystemParametersInfoW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
            WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
            WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    };

    use crate::{
        CaptureConfig, CaptureStatsSnapshot, CapturedInput, ControlEvent, InputEvent, Result,
        WindowsInputError, map_key,
    };
    use edge_geometry::{Point, apply_remote_motion, clamp};
    use edge_protocol::{MouseButton, ReleaseReason};

    const SM_XVIRTUALSCREEN: i32 = 76;
    const SM_YVIRTUALSCREEN: i32 = 77;
    const SM_CXVIRTUALSCREEN: i32 = 78;
    const SM_CYVIRTUALSCREEN: i32 = 79;
    const LLKHF_EXTENDED: u32 = 0x01;
    const VK_CONTROL: u32 = 0x11;
    const VK_MENU: u32 = 0x12;
    const VK_LCONTROL: u32 = 0xa2;
    const VK_RCONTROL: u32 = 0xa3;
    const VK_LMENU: u32 = 0xa4;
    const VK_RMENU: u32 = 0xa5;
    const VK_PAUSE: u32 = 0x13;
    const VK_ESCAPE: u32 = 0x1b;
    const XBUTTON1: u16 = 0x0001;
    const XBUTTON2: u16 = 0x0002;
    const WHEEL_DELTA: f64 = 120.0;
    const REMOTE_ENTRY_PADDING: f64 = 32.0;

    static STATE: OnceLock<Mutex<CaptureState>> = OnceLock::new();
    static CAPTURE_STATS: CaptureStats = CaptureStats::new();

    pub fn force_release_to_local() {
        let Some(state) = STATE.get() else {
            return;
        };
        let mut state = state.lock().expect("capture state poisoned");
        state.release_to_local(ReleaseReason::PeerDisconnected);
        state.show_source_cursor();
    }

    pub fn stats_snapshot() -> CaptureStatsSnapshot {
        CAPTURE_STATS.snapshot()
    }

    pub fn start(config: CaptureConfig) -> Result<mpsc::Receiver<CapturedInput>> {
        let (sender, receiver) = mpsc::channel();
        CAPTURE_STATS.active.store(false, Ordering::Relaxed);
        let state = CaptureState {
            sender,
            config,
            local_bounds: LocalBounds::query(),
            active: false,
            anchor: POINT { x: 0, y: 0 },
            remote_cursor: Point { x: 0.0, y: 0.0 },
            ctrl_down: false,
            alt_down: false,
            cursor_hidden: false,
        };

        if let Some(existing) = STATE.get() {
            let mut existing = existing.lock().expect("capture state poisoned");
            existing.show_source_cursor();
            *existing = state;
            tracing::info!("Windows input capture hooks reused");
            return Ok(receiver);
        }

        STATE
            .set(Mutex::new(state))
            .map_err(|_| WindowsInputError::CaptureAlreadyRunning)?;

        thread::Builder::new()
            .name("edge-kvm-input-hooks".to_string())
            .spawn(run_hook_thread)
            .map_err(|err| WindowsInputError::Capture(err.to_string()))?;

        Ok(receiver)
    }

    fn run_hook_thread() {
        unsafe {
            let instance = GetModuleHandleW(null_mut());
            let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0);
            let keyboard_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0);

            if mouse_hook.is_null() || keyboard_hook.is_null() {
                tracing::error!("failed to install low-level Windows input hooks");
                return;
            }

            tracing::info!("Windows input capture hooks installed");
            let mut message = MSG::default();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code != HC_ACTION as i32 {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        CAPTURE_STATS
            .mouse_hook_events
            .fetch_add(1, Ordering::Relaxed);
        let Some(state) = STATE.get() else {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        };
        let mut state = state.lock().expect("capture state poisoned");
        let message = wparam as u32;

        if !state.active {
            if message == WM_MOUSEMOVE && state.at_activation_edge(mouse.pt) {
                state.enter_remote(mouse.pt);
                return 1;
            }
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        if mouse.flags & LLMHF_INJECTED != 0 {
            return 1;
        }

        match message {
            WM_MOUSEMOVE => {
                state.keep_source_cursor_hidden();
                state.remote_motion(mouse.pt);
            }
            WM_LBUTTONDOWN => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Left,
                down: true,
            }),
            WM_LBUTTONUP => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Left,
                down: false,
            }),
            WM_RBUTTONDOWN => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Right,
                down: true,
            }),
            WM_RBUTTONUP => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Right,
                down: false,
            }),
            WM_MBUTTONDOWN => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Middle,
                down: true,
            }),
            WM_MBUTTONUP => state.send_input(InputEvent::PointerButton {
                button: MouseButton::Middle,
                down: false,
            }),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if let Some(button) = x_button(mouse.mouseData) {
                    state.send_input(InputEvent::PointerButton {
                        button,
                        down: message == WM_XBUTTONDOWN,
                    });
                }
            }
            WM_MOUSEWHEEL => {
                let delta = high_word_signed(mouse.mouseData) as f64 / WHEEL_DELTA;
                state.send_input(InputEvent::PointerWheel { x: 0.0, y: delta });
            }
            WM_MOUSEHWHEEL => {
                let delta = high_word_signed(mouse.mouseData) as f64 / WHEEL_DELTA;
                state.send_input(InputEvent::PointerWheel { x: delta, y: 0.0 });
            }
            _ => {}
        }

        1
    }

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code != HC_ACTION as i32 {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        CAPTURE_STATS
            .keyboard_hook_events
            .fetch_add(1, Ordering::Relaxed);
        let Some(state) = STATE.get() else {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        };
        let mut state = state.lock().expect("capture state poisoned");
        let message = wparam as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let up = message == WM_KEYUP || message == WM_SYSKEYUP;

        if down || up {
            state.update_modifier(keyboard.vkCode, down);
        }

        if !state.active {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        if down && state.ctrl_down && state.alt_down && is_release_hotkey(keyboard.vkCode) {
            state.release_to_local(ReleaseReason::Hotkey);
            return 1;
        }

        if down || up {
            state.keep_source_cursor_hidden();
            let scan_code = keyboard.scanCode as u16;
            let extended = keyboard.flags & LLKHF_EXTENDED != 0;
            match map_key(scan_code, extended) {
                Ok(evdev_code) => state.send_input(InputEvent::Key { evdev_code, down }),
                Err(err) => {
                    CAPTURE_STATS.unmapped_keys.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        %err,
                        scan_code,
                        extended,
                        vk_code = keyboard.vkCode,
                        "ignoring unmapped key"
                    );
                }
            }
        }

        1
    }

    struct CaptureState {
        sender: mpsc::Sender<CapturedInput>,
        config: CaptureConfig,
        local_bounds: LocalBounds,
        active: bool,
        anchor: POINT,
        remote_cursor: Point,
        ctrl_down: bool,
        alt_down: bool,
        cursor_hidden: bool,
    }

    impl CaptureState {
        fn at_activation_edge(&self, point: POINT) -> bool {
            match self.config.edge {
                Edge::Left => point.x <= self.local_bounds.left,
                Edge::Right => point.x >= self.local_bounds.right(),
                Edge::Top => point.y <= self.local_bounds.top,
                Edge::Bottom => point.y >= self.local_bounds.bottom(),
            }
        }

        fn enter_remote(&mut self, point: POINT) {
            self.active = true;
            CAPTURE_STATS.active.store(true, Ordering::Relaxed);
            CAPTURE_STATS.enter_events.fetch_add(1, Ordering::Relaxed);
            self.anchor = self.local_bounds.anchor_for(self.config.edge, point);
            self.remote_cursor = self.remote_start(point);
            self.send_control(ControlEvent::EnterRemote {
                edge: self.config.edge,
                normalized_y: self.normalized_perpendicular(point),
            });
            unsafe {
                SetCursorPos(self.anchor.x, self.anchor.y);
            }
            self.hide_source_cursor();
            tracing::info!(edge = ?self.config.edge, "entered remote control");
        }

        fn remote_motion(&mut self, point: POINT) {
            let dx = f64::from(point.x - self.anchor.x);
            let dy = f64::from(point.y - self.anchor.y);
            unsafe {
                SetCursorPos(self.anchor.x, self.anchor.y);
            }

            if dx == 0.0 && dy == 0.0 {
                return;
            }

            self.remote_cursor =
                apply_remote_motion(self.remote_cursor, dx, dy, self.config.remote_size);
            if self.exits_remote() {
                CAPTURE_STATS
                    .return_edge_hits
                    .fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    edge = ?self.config.edge,
                    remote_x = self.remote_cursor.x,
                    remote_y = self.remote_cursor.y,
                    "remote cursor reached return edge; releasing to local"
                );
                self.release_to_local(ReleaseReason::UserRequest);
                return;
            }

            self.send_input(InputEvent::PointerMotion { dx, dy });
        }

        fn release_to_local(&mut self, reason: ReleaseReason) {
            if !self.active {
                return;
            }
            self.active = false;
            CAPTURE_STATS.active.store(false, Ordering::Relaxed);
            CAPTURE_STATS.release_events.fetch_add(1, Ordering::Relaxed);
            self.send_input(InputEvent::AllKeysUp);
            self.send_control(ControlEvent::ReleaseToLocal { reason });
            self.show_source_cursor();
            let restore = self.local_restore();
            unsafe {
                SetCursorPos(restore.x, restore.y);
            }
            tracing::info!(?reason, "released remote control");
        }

        fn update_modifier(&mut self, vk_code: u32, down: bool) {
            match vk_code {
                VK_CONTROL | VK_LCONTROL | VK_RCONTROL => self.ctrl_down = down,
                VK_MENU | VK_LMENU | VK_RMENU => self.alt_down = down,
                _ => {}
            }
        }

        fn hide_source_cursor(&mut self) {
            if self.cursor_hidden {
                return;
            }
            unsafe { while ShowCursor(0) >= 0 {} }
            hide_system_cursors();
            self.cursor_hidden = true;
        }

        fn keep_source_cursor_hidden(&mut self) {
            if !self.cursor_hidden {
                self.hide_source_cursor();
            }
        }

        fn show_source_cursor(&mut self) {
            if !self.cursor_hidden {
                return;
            }
            restore_system_cursors();
            unsafe { while ShowCursor(1) < 0 {} }
            self.cursor_hidden = false;
        }

        fn remote_start(&self, point: POINT) -> Point {
            let normalized = f64::from(self.normalized_perpendicular(point));
            let remote = self.config.remote_size;
            let x_padding = remote_entry_padding(remote.width);
            let y_padding = remote_entry_padding(remote.height);
            match self.config.edge {
                Edge::Left => Point {
                    x: f64::from(remote.width.saturating_sub(1)) - x_padding,
                    y: normalized * f64::from(remote.height.saturating_sub(1)),
                },
                Edge::Right => Point {
                    x: x_padding,
                    y: normalized * f64::from(remote.height.saturating_sub(1)),
                },
                Edge::Top => Point {
                    x: normalized * f64::from(remote.width.saturating_sub(1)),
                    y: f64::from(remote.height.saturating_sub(1)) - y_padding,
                },
                Edge::Bottom => Point {
                    x: normalized * f64::from(remote.width.saturating_sub(1)),
                    y: y_padding,
                },
            }
        }

        fn exits_remote(&self) -> bool {
            let remote = self.config.remote_size;
            match self.config.edge {
                Edge::Left => self.remote_cursor.x >= f64::from(remote.width.saturating_sub(1)),
                Edge::Right => self.remote_cursor.x <= 0.0,
                Edge::Top => self.remote_cursor.y >= f64::from(remote.height.saturating_sub(1)),
                Edge::Bottom => self.remote_cursor.y <= 0.0,
            }
        }

        fn local_restore(&self) -> POINT {
            let normalized = self.remote_normalized_perpendicular();
            self.local_bounds.restore_for(self.config.edge, normalized)
        }

        fn normalized_perpendicular(&self, point: POINT) -> f32 {
            match self.config.edge {
                Edge::Left | Edge::Right => self.local_bounds.normalized_y(f64::from(point.y)),
                Edge::Top | Edge::Bottom => self.local_bounds.normalized_x(f64::from(point.x)),
            }
        }

        fn remote_normalized_perpendicular(&self) -> f32 {
            let remote = self.config.remote_size;
            match self.config.edge {
                Edge::Left | Edge::Right => normalized_axis(self.remote_cursor.y, remote.height),
                Edge::Top | Edge::Bottom => normalized_axis(self.remote_cursor.x, remote.width),
            }
        }

        fn send_input(&self, event: InputEvent) {
            if self.sender.send(CapturedInput::Input(event)).is_ok() {
                CAPTURE_STATS.input_events.fetch_add(1, Ordering::Relaxed);
            } else {
                CAPTURE_STATS.send_failures.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn send_control(&self, event: ControlEvent) {
            if self.sender.send(CapturedInput::Control(event)).is_ok() {
                CAPTURE_STATS.control_events.fetch_add(1, Ordering::Relaxed);
            } else {
                CAPTURE_STATS.send_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct CaptureStats {
        active: AtomicBool,
        mouse_hook_events: AtomicU64,
        keyboard_hook_events: AtomicU64,
        input_events: AtomicU64,
        control_events: AtomicU64,
        enter_events: AtomicU64,
        release_events: AtomicU64,
        return_edge_hits: AtomicU64,
        send_failures: AtomicU64,
        unmapped_keys: AtomicU64,
    }

    impl CaptureStats {
        const fn new() -> Self {
            Self {
                active: AtomicBool::new(false),
                mouse_hook_events: AtomicU64::new(0),
                keyboard_hook_events: AtomicU64::new(0),
                input_events: AtomicU64::new(0),
                control_events: AtomicU64::new(0),
                enter_events: AtomicU64::new(0),
                release_events: AtomicU64::new(0),
                return_edge_hits: AtomicU64::new(0),
                send_failures: AtomicU64::new(0),
                unmapped_keys: AtomicU64::new(0),
            }
        }

        fn snapshot(&self) -> CaptureStatsSnapshot {
            CaptureStatsSnapshot {
                active: self.active.load(Ordering::Relaxed),
                mouse_hook_events: self.mouse_hook_events.load(Ordering::Relaxed),
                keyboard_hook_events: self.keyboard_hook_events.load(Ordering::Relaxed),
                input_events: self.input_events.load(Ordering::Relaxed),
                control_events: self.control_events.load(Ordering::Relaxed),
                enter_events: self.enter_events.load(Ordering::Relaxed),
                release_events: self.release_events.load(Ordering::Relaxed),
                return_edge_hits: self.return_edge_hits.load(Ordering::Relaxed),
                send_failures: self.send_failures.load(Ordering::Relaxed),
                unmapped_keys: self.unmapped_keys.load(Ordering::Relaxed),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct LocalBounds {
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    }

    impl LocalBounds {
        fn query() -> Self {
            unsafe {
                Self {
                    left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                    top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                    width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
                }
            }
        }

        fn right(&self) -> i32 {
            self.left + self.width.saturating_sub(1)
        }

        fn bottom(&self) -> i32 {
            self.top + self.height.saturating_sub(1)
        }

        fn anchor_for(&self, edge: Edge, point: POINT) -> POINT {
            match edge {
                Edge::Left => POINT {
                    x: self.left + 2,
                    y: point.y.clamp(self.top, self.bottom()),
                },
                Edge::Right => POINT {
                    x: self.right() - 2,
                    y: point.y.clamp(self.top, self.bottom()),
                },
                Edge::Top => POINT {
                    x: point.x.clamp(self.left, self.right()),
                    y: self.top + 2,
                },
                Edge::Bottom => POINT {
                    x: point.x.clamp(self.left, self.right()),
                    y: self.bottom() - 2,
                },
            }
        }

        fn restore_for(&self, edge: Edge, normalized: f32) -> POINT {
            match edge {
                Edge::Left => POINT {
                    x: self.left + 3,
                    y: self.y_at(normalized),
                },
                Edge::Right => POINT {
                    x: self.right() - 3,
                    y: self.y_at(normalized),
                },
                Edge::Top => POINT {
                    x: self.x_at(normalized),
                    y: self.top + 3,
                },
                Edge::Bottom => POINT {
                    x: self.x_at(normalized),
                    y: self.bottom() - 3,
                },
            }
        }

        fn normalized_x(&self, x: f64) -> f32 {
            normalized_axis(x - f64::from(self.left), self.width.max(1) as u32)
        }

        fn normalized_y(&self, y: f64) -> f32 {
            normalized_axis(y - f64::from(self.top), self.height.max(1) as u32)
        }

        fn x_at(&self, normalized: f32) -> i32 {
            let x = f64::from(self.left)
                + f64::from(self.width.saturating_sub(1)) * f64::from(normalized);
            clamp(x, f64::from(self.left), f64::from(self.right())).round() as i32
        }

        fn y_at(&self, normalized: f32) -> i32 {
            let y = f64::from(self.top)
                + f64::from(self.height.saturating_sub(1)) * f64::from(normalized);
            clamp(y, f64::from(self.top), f64::from(self.bottom())).round() as i32
        }
    }

    fn normalized_axis(pos: f64, extent: u32) -> f32 {
        if extent <= 1 {
            return 0.0;
        }
        let max = f64::from(extent - 1);
        (clamp(pos, 0.0, max) / max) as f32
    }

    fn remote_entry_padding(extent: u32) -> f64 {
        let max = f64::from(extent.saturating_sub(1));
        clamp(REMOTE_ENTRY_PADDING, 1.0, max)
    }

    fn high_word_signed(value: u32) -> i16 {
        ((value >> 16) & 0xffff) as u16 as i16
    }

    fn x_button(mouse_data: u32) -> Option<MouseButton> {
        match ((mouse_data >> 16) & 0xffff) as u16 {
            XBUTTON1 => Some(MouseButton::Back),
            XBUTTON2 => Some(MouseButton::Forward),
            _ => None,
        }
    }

    fn is_release_hotkey(vk_code: u32) -> bool {
        vk_code == VK_PAUSE || vk_code == VK_ESCAPE
    }

    fn hide_system_cursors() {
        const CURSOR_IDS: &[u32] = &[
            OCR_NORMAL,
            OCR_IBEAM,
            OCR_WAIT,
            OCR_CROSS,
            OCR_UP,
            OCR_SIZEALL,
            OCR_SIZENESW,
            OCR_SIZENS,
            OCR_SIZENWSE,
            OCR_SIZEWE,
            OCR_NO,
            OCR_HAND,
            OCR_APPSTARTING,
            OCR_HELP,
        ];

        for cursor_id in CURSOR_IDS {
            let Some(cursor) = create_blank_cursor() else {
                tracing::warn!("failed to create blank cursor for remote capture");
                return;
            };
            if unsafe { SetSystemCursor(cursor, *cursor_id) } == 0 {
                unsafe {
                    DestroyCursor(cursor);
                }
                tracing::warn!(cursor_id, "failed to replace system cursor");
                restore_system_cursors();
                return;
            }
        }
    }

    fn restore_system_cursors() {
        unsafe {
            SystemParametersInfoW(SPI_SETCURSORS, 0, null_mut(), 0);
        }
    }

    fn create_blank_cursor() -> Option<*mut std::ffi::c_void> {
        let and_plane = [0xff_u8; 128];
        let xor_plane = [0_u8; 128];
        let cursor = unsafe {
            CreateCursor(
                null_mut(),
                0,
                0,
                32,
                32,
                and_plane.as_ptr().cast(),
                xor_plane.as_ptr().cast(),
            )
        };
        (!cursor.is_null()).then_some(cursor)
    }
}

#[cfg(windows)]
mod tray {
    use std::{ffi::c_void, mem::size_of, ptr::null_mut, sync::OnceLock};

    use windows_sys::Win32::{
        Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
                NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
                IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostQuitMessage,
                RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
                TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY,
                WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use crate::{Result, WindowsInputError};

    const TRAY_ID: u32 = 1;
    const WM_TRAY_ICON: u32 = WM_APP + 1;
    const ID_RELEASE: usize = 1001;
    const ID_QUIT: usize = 1002;

    static TRAY_STATUS: OnceLock<Vec<u16>> = OnceLock::new();

    pub fn run(status: &str) -> Result<()> {
        unsafe {
            let _ = TRAY_STATUS.set(to_wide(status));

            let instance = GetModuleHandleW(null_mut());
            if instance.is_null() {
                return Err(WindowsInputError::Tray(
                    "GetModuleHandleW failed".to_string(),
                ));
            }

            let class_name = to_wide("EdgeKvmTrayWindow");
            let window_name = to_wide("edge-kvm");
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };

            if RegisterClassW(&window_class) == 0 {
                return Err(WindowsInputError::Tray("RegisterClassW failed".to_string()));
            }

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                null_mut(),
                null_mut(),
                instance,
                null_mut::<c_void>(),
            );
            if hwnd.is_null() {
                return Err(WindowsInputError::Tray(
                    "CreateWindowExW failed".to_string(),
                ));
            }

            add_tray_icon(hwnd, status)?;

            let mut message = MSG::default();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            Ok(())
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match message {
                WM_TRAY_ICON => {
                    if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_LBUTTONDBLCLK {
                        show_menu(hwnd);
                    }
                    0
                }
                WM_COMMAND => {
                    match wparam & 0xffff {
                        ID_RELEASE => {
                            tracing::info!("release requested from tray");
                        }
                        ID_QUIT => {
                            remove_tray_icon(hwnd);
                            DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                    0
                }
                WM_DESTROY => {
                    remove_tray_icon(hwnd);
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
    }

    fn add_tray_icon(hwnd: HWND, status: &str) -> Result<()> {
        let mut data = notify_icon_data(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY_ICON;
        data.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        copy_wide("edge-kvm", status, &mut data.szTip);

        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            let error = unsafe { GetLastError() };
            return Err(WindowsInputError::Tray(format!(
                "Shell_NotifyIconW(NIM_ADD) failed with Win32 error {error}"
            )));
        }

        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        if unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) } == 0 {
            let error = unsafe { GetLastError() };
            return Err(WindowsInputError::Tray(format!(
                "Shell_NotifyIconW(NIM_SETVERSION) failed with Win32 error {error}"
            )));
        }
        Ok(())
    }

    fn remove_tray_icon(hwnd: HWND) {
        let data = notify_icon_data(hwnd);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    fn show_menu(hwnd: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }

        let status = TRAY_STATUS
            .get()
            .map(|value| value.as_ptr())
            .unwrap_or_else(|| to_wide("edge-kvm").leak().as_ptr());
        let release = to_wide("Release control");
        let quit = to_wide("Quit");

        unsafe {
            AppendMenuW(menu, MF_STRING, 0, status);
            AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
            AppendMenuW(menu, MF_STRING, ID_RELEASE, release.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());

            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 {
                SetForegroundWindow(hwnd);
                TrackPopupMenu(
                    menu,
                    TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null_mut(),
                );
            }

            DestroyMenu(menu);
        }
    }

    fn notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn copy_wide(prefix: &str, status: &str, target: &mut [u16]) {
        let text = format!("{prefix}: {status}");
        let wide = to_wide(&text);
        let count = wide
            .len()
            .saturating_sub(1)
            .min(target.len().saturating_sub(1));
        target[..count].copy_from_slice(&wide[..count]);
        target[count] = 0;
    }

    fn to_wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
