use std::sync::mpsc;

use edge_common::GameCompatibilityMode;
use edge_geometry::Size;
use edge_keymap::{WindowsScanCode, windows_scancode_to_evdev};
use edge_protocol::{ControlEvent, Edge, InputEvent, ReleaseReason};

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
    pub game_compatibility: GameCompatibilityMode,
}

#[derive(Debug, Clone)]
pub enum CapturedInput {
    Input(InputEvent),
    Control(ControlEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsTrayCommand {
    OpenSettings,
    ReleaseControl,
    ToggleAudio,
    Quit,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStatsSnapshot {
    pub active: bool,
    pub suspended: bool,
    pub mouse_hook_installed: bool,
    pub mouse_hook_events: u64,
    pub keyboard_hook_events: u64,
    pub raw_mouse_events: u64,
    pub raw_keyboard_events: u64,
    pub raw_input_repairs: u64,
    pub mouse_hook_repairs: u64,
    pub keyboard_hook_repairs: u64,
    pub input_pipeline_restarts: u64,
    pub callback_contention_drops: u64,
    pub input_supervisor_checks: u64,
    pub system_last_input_tick: u32,
    pub raw_worker_thread_id: u32,
    pub hook_worker_thread_id: u32,
    pub input_events: u64,
    pub control_events: u64,
    pub enter_events: u64,
    pub release_events: u64,
    pub return_edge_hits: u64,
    pub game_guard_blocks: u64,
    pub game_guard_releases: u64,
    pub suspend_toggles: u64,
    pub suspend_blocks: u64,
    pub suspend_auto_resumes: u64,
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
pub fn run_tray(status: &str, commands: mpsc::Sender<WindowsTrayCommand>) -> Result<()> {
    tray::run(status, commands)
}

#[cfg(not(windows))]
pub fn run_tray(_status: &str, _commands: mpsc::Sender<WindowsTrayCommand>) -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn update_tray_status(status: &str) -> Result<()> {
    tray::update_status(status)
}

#[cfg(not(windows))]
pub fn update_tray_status(_status: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn force_release_to_local() {
    capture::disable()
}

#[cfg(not(windows))]
pub fn force_release_to_local() {}

#[cfg(windows)]
pub fn release_to_local(reason: ReleaseReason) {
    capture::release_to_local(reason)
}

#[cfg(windows)]
pub fn handle_receiver_release(reason: ReleaseReason) -> bool {
    capture::handle_receiver_release(reason)
}

#[cfg(not(windows))]
pub fn handle_receiver_release(_reason: ReleaseReason) -> bool {
    false
}

#[cfg(not(windows))]
pub fn release_to_local(_reason: ReleaseReason) {}

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
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use edge_common::GameCompatibilityMode;
    use edge_protocol::Edge;
    use windows_sys::Win32::{
        Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::{
                GetRawInputData,
                KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
                MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
                RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
            },
            WindowsAndMessaging::{
                CallNextHookEx, CreateCursor, CreateWindowExW, DefWindowProcW, DestroyCursor,
                DispatchMessageW, GetClipCursor, GetForegroundWindow, GetMessageW,
                GetSystemMetrics, GetWindowRect, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLMHF_INJECTED,
                MSG, MSLLHOOKSTRUCT, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM,
                OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE,
                OCR_SIZEWE, OCR_UP, OCR_WAIT, PostThreadMessageW, RegisterClassW, SPI_SETCURSORS,
                SetCursorPos, SetSystemCursor, SetWindowsHookExW, ShowCursor,
                SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
                WH_MOUSE_LL, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
                WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
                WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
                WM_XBUTTONUP, WNDCLASSW, WS_POPUP,
            },
        },
    };

    use crate::{
        CaptureConfig, CaptureStatsSnapshot, CapturedInput, ControlEvent, InputEvent, Result,
        WindowsInputError, map_key,
    };
    use edge_geometry::{Point, Size, apply_remote_motion, clamp};
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
    const VK_G: u32 = 0x47;
    const XBUTTON1: u16 = 0x0001;
    const XBUTTON2: u16 = 0x0002;
    const WHEEL_DELTA: f64 = 120.0;
    const REMOTE_ENTRY_PADDING: f64 = 32.0;
    const REMOTE_RETURN_MARGIN: f64 = 12.0;
    const GAME_GUARD_CHECK_INTERVAL: Duration = Duration::from_millis(250);
    const RELEASE_REENTRY_COOLDOWN: Duration = Duration::from_millis(750);
    const INPUT_SUPERVISOR_INTERVAL: Duration = Duration::from_secs(1);
    const INPUT_STALL_CONFIRMATIONS: u8 = 2;
    const INPUT_RESTART_COOLDOWN: Duration = Duration::from_secs(8);
    const FULLSCREEN_TOLERANCE_PX: i32 = 2;

    static STATE: OnceLock<Mutex<CaptureState>> = OnceLock::new();
    static MOUSE_HOOK: Mutex<isize> = Mutex::new(0);
    static KEYBOARD_HOOK: Mutex<isize> = Mutex::new(0);
    static RAW_MOUSE_EVENTS: AtomicU64 = AtomicU64::new(0);
    static RAW_KEYBOARD_EVENTS: AtomicU64 = AtomicU64::new(0);
    static RAW_WORKER_GENERATION: AtomicU64 = AtomicU64::new(0);
    static RAW_WORKER_THREAD_ID: AtomicU32 = AtomicU32::new(0);
    static HOOK_WORKER_THREAD_ID: AtomicU32 = AtomicU32::new(0);
    static INPUT_SUPERVISOR_STARTED: AtomicBool = AtomicBool::new(false);
    static CAPTURE_STATS: CaptureStats = CaptureStats::new();

    #[derive(Clone, Copy)]
    struct InputSupervisorSnapshot {
        raw_mouse: u64,
        raw_keyboard: u64,
        hook_mouse: u64,
        hook_keyboard: u64,
        system_last_input: Option<u32>,
    }

    impl InputSupervisorSnapshot {
        fn current() -> Self {
            Self {
                raw_mouse: RAW_MOUSE_EVENTS.load(Ordering::Relaxed),
                raw_keyboard: RAW_KEYBOARD_EVENTS.load(Ordering::Relaxed),
                hook_mouse: CAPTURE_STATS.mouse_hook_events.load(Ordering::Relaxed),
                hook_keyboard: CAPTURE_STATS.keyboard_hook_events.load(Ordering::Relaxed),
                system_last_input: system_last_input_tick(),
            }
        }

        fn raw_advanced(self, previous: Self) -> bool {
            self.raw_mouse > previous.raw_mouse || self.raw_keyboard > previous.raw_keyboard
        }

        fn hooks_advanced(self, previous: Self) -> bool {
            self.hook_mouse > previous.hook_mouse || self.hook_keyboard > previous.hook_keyboard
        }

        fn system_input_advanced(self, previous: Self) -> bool {
            matches!(
                (self.system_last_input, previous.system_last_input),
                (Some(current), Some(old)) if current != old
            )
        }
    }

    pub fn release_to_local(reason: ReleaseReason) {
        let Some(state) = STATE.get() else {
            return;
        };
        let mut state = state.lock().expect("capture state poisoned");
        state.release_to_local(reason);
        state.show_source_cursor();
    }

    pub fn handle_receiver_release(reason: ReleaseReason) -> bool {
        let Some(state) = STATE.get() else {
            return false;
        };
        let mut state = state.lock().expect("capture state poisoned");
        if !state.active {
            return false;
        }
        if reason == ReleaseReason::UserRequest && !state.remote_at_return_edge() {
            tracing::warn!(
                edge = ?state.config.edge,
                x = state.remote_cursor.x,
                y = state.remote_cursor.y,
                "ignored implausible receiver release away from the return edge"
            );
            return false;
        }
        state.release_to_local(reason);
        true
    }

    pub fn disable() {
        let Some(state) = STATE.get() else {
            return;
        };
        let mut state = state.lock().expect("capture state poisoned");
        state.enabled = false;
        state.release_to_local(ReleaseReason::PeerDisconnected);
        state.show_source_cursor();
        tracing::info!("Windows edge capture disabled until the next connection");
    }

    pub fn stats_snapshot() -> CaptureStatsSnapshot {
        CAPTURE_STATS.snapshot()
    }

    pub fn start(config: CaptureConfig) -> Result<mpsc::Receiver<CapturedInput>> {
        let (sender, receiver) = mpsc::channel();
        CAPTURE_STATS.active.store(false, Ordering::Relaxed);
        CAPTURE_STATS.suspended.store(false, Ordering::Relaxed);
        let local_bounds = LocalBounds::query();
        let state = CaptureState {
            sender,
            config,
            virtual_local_cursor: local_bounds.center(),
            local_bounds,
            active: false,
            enabled: true,
            anchor: POINT { x: 0, y: 0 },
            remote_cursor: Point { x: 0.0, y: 0.0 },
            ctrl_down: false,
            alt_down: false,
            capture_suspended: false,
            suspend_foreground: None,
            cursor_hidden: false,
            game_guard: GameGuard::default(),
            raw_absolute_position: None,
            last_raw_input: None,
            activation_blocked_until: Instant::now(),
        };

        if let Some(existing) = STATE.get() {
            {
                let mut existing = existing.lock().expect("capture state poisoned");
                existing.show_source_cursor();
                *existing = state;
            }
            restart_input_workers("connection restarted")?;
            tracing::info!("Windows input workers refreshed for the new connection");
            return Ok(receiver);
        }

        STATE
            .set(Mutex::new(state))
            .map_err(|_| WindowsInputError::CaptureAlreadyRunning)?;

        spawn_raw_input_worker()?;
        spawn_hook_worker()?;
        start_input_supervisor()?;

        Ok(receiver)
    }

    fn spawn_raw_input_worker() -> Result<()> {
        let old_thread = RAW_WORKER_THREAD_ID.swap(0, Ordering::AcqRel);
        if old_thread != 0 {
            unsafe { PostThreadMessageW(old_thread, WM_QUIT, 0, 0) };
        }

        let generation = RAW_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        thread::Builder::new()
            .name(format!("edge-kvm-raw-input-{generation}"))
            .spawn(move || run_raw_input_thread(generation))
            .map_err(|err| WindowsInputError::Capture(err.to_string()))?;
        Ok(())
    }

    fn run_raw_input_thread(generation: u64) {
        RAW_WORKER_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        unsafe {
            let instance = GetModuleHandleW(null_mut());
            if !install_raw_input_window(instance, generation) {
                return;
            }

            tracing::info!(generation, "Windows Raw Input worker started");
            let mut message = MSG::default();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn spawn_hook_worker() -> Result<()> {
        stop_hook_worker();
        thread::Builder::new()
            .name("edge-kvm-input-hooks".to_string())
            .spawn(run_hook_thread)
            .map_err(|err| WindowsInputError::Capture(err.to_string()))?;
        Ok(())
    }

    fn stop_hook_worker() {
        let old_thread = HOOK_WORKER_THREAD_ID.swap(0, Ordering::AcqRel);
        if old_thread != 0 {
            unsafe { PostThreadMessageW(old_thread, WM_QUIT, 0, 0) };
        }
        uninstall_mouse_hook();
        uninstall_keyboard_hook();
    }

    fn run_hook_thread() {
        HOOK_WORKER_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        unsafe {
            let instance = GetModuleHandleW(null_mut());
            let mouse_hook = install_mouse_hook(instance);
            let keyboard_hook = install_keyboard_hook(instance);

            if mouse_hook == 0 || keyboard_hook == 0 {
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

    unsafe fn install_raw_input_window(instance: *mut std::ffi::c_void, generation: u64) -> bool {
        let class_name: Vec<u16> = format!("EdgeKvmRawInputWindow{generation}\0")
            .encode_utf16()
            .collect();
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(raw_input_window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&window_class) } == 0 {
            tracing::warn!(
                error = unsafe { GetLastError() },
                generation,
                "failed to register Windows Raw Input window class"
            );
            return false;
        }

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            tracing::warn!(
                error = unsafe { GetLastError() },
                generation,
                "failed to create Windows Raw Input window"
            );
            return false;
        }

        if !register_raw_input_devices(hwnd) {
            return false;
        }

        tracing::info!(generation, "Windows Raw Input registration enabled");
        true
    }

    fn register_raw_input_devices(hwnd: HWND) -> bool {
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        let registered = unsafe {
            RegisterRawInputDevices(
                devices.as_ptr(),
                devices.len() as u32,
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        if registered == 0 {
            tracing::warn!(
                error = unsafe { GetLastError() },
                "failed to register Windows Raw Input devices"
            );
            return false;
        }
        true
    }

    unsafe extern "system" fn raw_input_window_proc(
        hwnd: HWND,
        message: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_INPUT {
            handle_raw_mouse_input(lparam);
        }
        unsafe { DefWindowProcW(hwnd, message, _wparam, lparam) }
    }

    fn handle_raw_mouse_input(lparam: LPARAM) {
        let mut input = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                lparam as _,
                RID_INPUT,
                (&mut input as *mut RAWINPUT).cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX {
            return;
        }

        if input.header.dwType == RIM_TYPEKEYBOARD {
            RAW_KEYBOARD_EVENTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if input.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        RAW_MOUSE_EVENTS.fetch_add(1, Ordering::Relaxed);

        let mouse = unsafe { input.data.mouse };
        let Some(state) = STATE.get() else {
            return;
        };
        let Ok(mut state) = state.try_lock() else {
            CAPTURE_STATS
                .callback_contention_drops
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        state.last_raw_input = Some(Instant::now());
        if !state.enabled {
            return;
        }
        if state.active {
            state.raw_remote_motion(
                mouse.lLastX,
                mouse.lLastY,
                mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0,
            );
        } else {
            state.raw_local_motion(
                mouse.lLastX,
                mouse.lLastY,
                mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0,
            );
        }
    }

    fn start_input_supervisor() -> Result<()> {
        if INPUT_SUPERVISOR_STARTED.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        thread::Builder::new()
            .name("edge-kvm-input-supervisor".to_string())
            .spawn(run_input_supervisor)
            .map_err(|err| WindowsInputError::Capture(err.to_string()))?;
        Ok(())
    }

    fn run_input_supervisor() {
        let mut previous = InputSupervisorSnapshot::current();
        let mut fully_stalled_ticks = 0_u8;
        let mut hook_stalled_ticks = 0_u8;
        let mut raw_stalled_ticks = 0_u8;
        let mut last_restart = Instant::now() - INPUT_RESTART_COOLDOWN;

        loop {
            thread::sleep(INPUT_SUPERVISOR_INTERVAL);
            CAPTURE_STATS
                .input_supervisor_checks
                .fetch_add(1, Ordering::Relaxed);
            let current = InputSupervisorSnapshot::current();
            let (enabled, suspended) = capture_enabled_and_suspended();
            if !enabled {
                previous = current;
                fully_stalled_ticks = 0;
                hook_stalled_ticks = 0;
                raw_stalled_ticks = 0;
                continue;
            }

            let hook_stalled = hook_needs_repair(
                current.raw_mouse,
                previous.raw_mouse,
                current.hook_mouse,
                previous.hook_mouse,
                !suspended,
            ) || hook_needs_repair(
                current.raw_keyboard,
                previous.raw_keyboard,
                current.hook_keyboard,
                previous.hook_keyboard,
                true,
            );

            if full_pipeline_stalled(current, previous) {
                fully_stalled_ticks = fully_stalled_ticks.saturating_add(1);
            } else {
                fully_stalled_ticks = 0;
            }
            if hook_stalled {
                hook_stalled_ticks = hook_stalled_ticks.saturating_add(1);
            } else {
                hook_stalled_ticks = 0;
            }
            if raw_input_stalled(current, previous) {
                raw_stalled_ticks = raw_stalled_ticks.saturating_add(1);
            } else {
                raw_stalled_ticks = 0;
            }

            let cooldown_complete = last_restart.elapsed() >= INPUT_RESTART_COOLDOWN;
            if cooldown_complete && fully_stalled_ticks >= INPUT_STALL_CONFIRMATIONS {
                tracing::warn!(
                    system_last_input = ?current.system_last_input,
                    "Windows reports input but the capture pipeline is stalled; replacing all input workers"
                );
                if restart_input_workers("system input advanced without capture events").is_ok() {
                    CAPTURE_STATS
                        .input_pipeline_restarts
                        .fetch_add(1, Ordering::Relaxed);
                    last_restart = Instant::now();
                }
                fully_stalled_ticks = 0;
                hook_stalled_ticks = 0;
                raw_stalled_ticks = 0;
            } else if cooldown_complete && hook_stalled_ticks >= INPUT_STALL_CONFIRMATIONS {
                tracing::warn!("Raw Input outpaced low-level hooks; replacing the hook worker");
                if restart_hook_worker().is_ok() {
                    last_restart = Instant::now();
                }
                hook_stalled_ticks = 0;
            } else if cooldown_complete && raw_stalled_ticks >= INPUT_STALL_CONFIRMATIONS {
                tracing::warn!(
                    "low-level hooks outpaced Raw Input; replacing the Raw Input worker"
                );
                if restart_raw_input_worker().is_ok() {
                    last_restart = Instant::now();
                }
                raw_stalled_ticks = 0;
            }

            previous = current;
        }
    }

    fn capture_enabled_and_suspended() -> (bool, bool) {
        STATE
            .get()
            .and_then(|state| {
                state
                    .try_lock()
                    .ok()
                    .map(|state| (state.enabled, state.capture_suspended))
            })
            .unwrap_or((false, false))
    }

    fn restart_input_workers(reason: &str) -> Result<()> {
        tracing::warn!(reason, "restarting the independent Windows input workers");
        restart_raw_input_worker()?;
        restart_hook_worker()?;
        Ok(())
    }

    fn restart_raw_input_worker() -> Result<()> {
        spawn_raw_input_worker()?;
        CAPTURE_STATS
            .raw_input_repairs
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn restart_hook_worker() -> Result<()> {
        spawn_hook_worker()?;
        CAPTURE_STATS
            .mouse_hook_repairs
            .fetch_add(1, Ordering::Relaxed);
        CAPTURE_STATS
            .keyboard_hook_repairs
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn system_last_input_tick() -> Option<u32> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        (unsafe { GetLastInputInfo(&mut info) } != 0).then_some(info.dwTime)
    }

    fn hook_needs_repair(
        raw_events: u64,
        previous_raw_events: u64,
        hook_events: u64,
        previous_hook_events: u64,
        enabled: bool,
    ) -> bool {
        enabled && raw_events > previous_raw_events && hook_events == previous_hook_events
    }

    fn full_pipeline_stalled(
        current: InputSupervisorSnapshot,
        previous: InputSupervisorSnapshot,
    ) -> bool {
        current.system_input_advanced(previous)
            && !current.raw_advanced(previous)
            && !current.hooks_advanced(previous)
    }

    fn raw_input_stalled(
        current: InputSupervisorSnapshot,
        previous: InputSupervisorSnapshot,
    ) -> bool {
        current.hooks_advanced(previous) && !current.raw_advanced(previous)
    }

    unsafe fn install_mouse_hook(instance: *mut std::ffi::c_void) -> isize {
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0) };
        let hook_id = hook as isize;
        if hook_id != 0 {
            *MOUSE_HOOK.lock().expect("mouse hook state poisoned") = hook_id;
        }
        hook_id
    }

    unsafe fn install_keyboard_hook(instance: *mut std::ffi::c_void) -> isize {
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0) };
        let hook_id = hook as isize;
        if hook_id != 0 {
            *KEYBOARD_HOOK.lock().expect("keyboard hook state poisoned") = hook_id;
        }
        hook_id
    }

    fn install_mouse_hook_if_needed() {
        let mut hook_id = MOUSE_HOOK.lock().expect("mouse hook state poisoned");
        if *hook_id != 0 {
            return;
        }

        unsafe {
            let instance = GetModuleHandleW(null_mut());
            let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0);
            if hook.is_null() {
                tracing::warn!("failed to reinstall low-level Windows mouse hook");
                return;
            }
            *hook_id = hook as isize;
            tracing::info!("Windows mouse hook reinstalled");
        }
    }

    fn uninstall_mouse_hook() {
        let mut mouse_hook = MOUSE_HOOK.lock().expect("mouse hook state poisoned");
        let hook = std::mem::take(&mut *mouse_hook);
        if hook == 0 {
            return;
        }

        if unsafe { UnhookWindowsHookEx(hook as HHOOK) } == 0 {
            tracing::warn!("failed to uninstall low-level Windows mouse hook");
        } else {
            tracing::info!("Windows mouse hook uninstalled");
        }
    }

    fn uninstall_keyboard_hook() {
        let mut keyboard_hook = KEYBOARD_HOOK.lock().expect("keyboard hook state poisoned");
        let hook = std::mem::take(&mut *keyboard_hook);
        if hook != 0 {
            if unsafe { UnhookWindowsHookEx(hook as HHOOK) } == 0 {
                tracing::warn!("failed to uninstall low-level Windows keyboard hook");
            } else {
                tracing::info!("Windows keyboard hook uninstalled");
            }
        }
    }

    fn mouse_hook_installed() -> bool {
        *MOUSE_HOOK.lock().expect("mouse hook state poisoned") != 0
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
        let Ok(mut state) = state.try_lock() else {
            CAPTURE_STATS
                .callback_contention_drops
                .fetch_add(1, Ordering::Relaxed);
            if CAPTURE_STATS.active.load(Ordering::Relaxed) {
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
        };
        let message = wparam as u32;

        if !state.enabled {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        state.refresh_capture_suspension();
        if state.capture_suspended {
            if state.active {
                tracing::info!("capture is suspended; releasing remote control");
                state.release_to_local(ReleaseReason::UserRequest);
            } else if message == WM_MOUSEMOVE && state.at_activation_edge(mouse.pt) {
                CAPTURE_STATS.suspend_blocks.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("capture suspension blocked edge activation");
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

        if state.game_guard_blocks_capture() {
            if state.active {
                CAPTURE_STATS
                    .game_guard_releases
                    .fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    "foreground fullscreen/captured app detected; releasing remote control"
                );
                state.release_to_local(ReleaseReason::UserRequest);
            } else if message == WM_MOUSEMOVE && state.at_activation_edge(mouse.pt) {
                CAPTURE_STATS
                    .game_guard_blocks
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!("foreground fullscreen/captured app blocked edge activation");
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

        if !state.active {
            if message == WM_MOUSEMOVE && !state.virtual_edge_required() {
                state.virtual_local_cursor = Point {
                    x: f64::from(mouse.pt.x),
                    y: f64::from(mouse.pt.y),
                };
            }
            if message == WM_MOUSEMOVE
                && state.activation_allowed()
                && state.at_activation_edge(mouse.pt)
            {
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
                if !state.raw_input_is_recent() {
                    state.remote_motion(mouse.pt);
                }
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
        let Ok(mut state) = state.try_lock() else {
            CAPTURE_STATS
                .callback_contention_drops
                .fetch_add(1, Ordering::Relaxed);
            if CAPTURE_STATS.active.load(Ordering::Relaxed) {
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
        };
        let message = wparam as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let up = message == WM_KEYUP || message == WM_SYSKEYUP;

        if !state.enabled {
            return unsafe {
                CallNextHookEx(
                    null_mut::<std::ffi::c_void>() as HHOOK,
                    code,
                    wparam,
                    lparam,
                )
            };
        }

        state.refresh_capture_suspension();
        if down || up {
            state.update_modifier(keyboard.vkCode, down);
        }

        if down && state.ctrl_down && state.alt_down && keyboard.vkCode == VK_G {
            state.toggle_capture_suspended();
            return 1;
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
        enabled: bool,
        anchor: POINT,
        remote_cursor: Point,
        virtual_local_cursor: Point,
        ctrl_down: bool,
        alt_down: bool,
        capture_suspended: bool,
        suspend_foreground: Option<isize>,
        cursor_hidden: bool,
        game_guard: GameGuard,
        raw_absolute_position: Option<(i32, i32)>,
        last_raw_input: Option<Instant>,
        activation_blocked_until: Instant,
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

        fn game_guard_blocks_capture(&mut self) -> bool {
            self.game_guard
                .blocks_capture(self.config.game_compatibility)
        }

        fn virtual_edge_required(&mut self) -> bool {
            self.game_guard
                .virtual_edge_required(self.config.game_compatibility)
        }

        fn activation_allowed(&self) -> bool {
            Instant::now() >= self.activation_blocked_until
        }

        fn toggle_capture_suspended(&mut self) {
            self.capture_suspended = !self.capture_suspended;
            self.suspend_foreground = self
                .capture_suspended
                .then(foreground_window_id)
                .filter(|id| *id != 0);
            CAPTURE_STATS
                .suspended
                .store(self.capture_suspended, Ordering::Relaxed);
            CAPTURE_STATS
                .suspend_toggles
                .fetch_add(1, Ordering::Relaxed);
            if self.capture_suspended {
                tracing::info!(
                    foreground = ?self.suspend_foreground,
                    "Windows edge capture suspended"
                );
                self.release_to_local(ReleaseReason::UserRequest);
                self.show_source_cursor();
                uninstall_mouse_hook();
            } else {
                install_mouse_hook_if_needed();
                tracing::info!("Windows edge capture resumed");
            }
        }

        fn refresh_capture_suspension(&mut self) {
            if !self.capture_suspended {
                return;
            }
            let Some(suspended_foreground) = self.suspend_foreground else {
                return;
            };
            let current_foreground = foreground_window_id();
            if current_foreground == 0 || current_foreground == suspended_foreground {
                return;
            }

            self.capture_suspended = false;
            self.suspend_foreground = None;
            CAPTURE_STATS.suspended.store(false, Ordering::Relaxed);
            install_mouse_hook_if_needed();
            CAPTURE_STATS
                .suspend_auto_resumes
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                suspended_foreground,
                current_foreground,
                "Windows edge capture auto-resumed after foreground changed"
            );
        }

        fn enter_remote(&mut self, point: POINT) {
            self.active = true;
            CAPTURE_STATS.active.store(true, Ordering::Relaxed);
            CAPTURE_STATS.enter_events.fetch_add(1, Ordering::Relaxed);
            self.anchor = self.local_bounds.anchor_for(self.config.edge, point);
            self.remote_cursor = self.remote_start(point);
            self.raw_absolute_position = None;
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

            self.send_input(InputEvent::PointerMotion { dx, dy });
            self.release_if_remote_return_edge();
        }

        fn raw_remote_motion(&mut self, x: i32, y: i32, absolute: bool) {
            let (dx, dy) = if absolute {
                let previous = self.raw_absolute_position.replace((x, y));
                let Some((previous_x, previous_y)) = previous else {
                    return;
                };
                (x - previous_x, y - previous_y)
            } else {
                self.raw_absolute_position = None;
                (x, y)
            };
            if dx == 0 && dy == 0 {
                return;
            }

            let dx = f64::from(dx);
            let dy = f64::from(dy);
            self.remote_cursor =
                apply_remote_motion(self.remote_cursor, dx, dy, self.config.remote_size);
            self.send_input(InputEvent::PointerMotion { dx, dy });
            self.release_if_remote_return_edge();
        }

        fn remote_at_return_edge(&self) -> bool {
            remote_cursor_at_return_edge(
                self.config.edge,
                self.remote_cursor,
                self.config.remote_size,
            )
        }

        fn release_if_remote_return_edge(&mut self) {
            if !self.remote_at_return_edge() {
                return;
            }
            CAPTURE_STATS
                .return_edge_hits
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(edge = ?self.config.edge, "controller-tracked cursor reached remote return edge");
            self.release_to_local(ReleaseReason::UserRequest);
        }

        fn raw_local_motion(&mut self, x: i32, y: i32, absolute: bool) {
            if !self.virtual_edge_required() {
                self.raw_absolute_position = None;
                return;
            }

            let (dx, dy) = if absolute {
                let previous = self.raw_absolute_position.replace((x, y));
                let Some((previous_x, previous_y)) = previous else {
                    return;
                };
                (x - previous_x, y - previous_y)
            } else {
                self.raw_absolute_position = None;
                (x, y)
            };
            if dx == 0 && dy == 0 {
                return;
            }

            self.virtual_local_cursor.x = clamp(
                self.virtual_local_cursor.x + f64::from(dx),
                f64::from(self.local_bounds.left),
                f64::from(self.local_bounds.right()),
            );
            self.virtual_local_cursor.y = clamp(
                self.virtual_local_cursor.y + f64::from(dy),
                f64::from(self.local_bounds.top),
                f64::from(self.local_bounds.bottom()),
            );
            let point = POINT {
                x: self.virtual_local_cursor.x.round() as i32,
                y: self.virtual_local_cursor.y.round() as i32,
            };
            if self.activation_allowed() && self.at_activation_edge(point) {
                tracing::info!(edge = ?self.config.edge, "game-mode virtual cursor reached activation edge");
                self.enter_remote(point);
            }
        }

        fn raw_input_is_recent(&self) -> bool {
            self.last_raw_input
                .is_some_and(|last_input| last_input.elapsed() < Duration::from_millis(250))
        }

        fn release_to_local(&mut self, reason: ReleaseReason) {
            if !self.active {
                return;
            }
            self.active = false;
            self.raw_absolute_position = None;
            self.activation_blocked_until = Instant::now() + RELEASE_REENTRY_COOLDOWN;
            CAPTURE_STATS.active.store(false, Ordering::Relaxed);
            CAPTURE_STATS.release_events.fetch_add(1, Ordering::Relaxed);
            self.send_input(InputEvent::AllKeysUp);
            self.send_control(ControlEvent::ReleaseToLocal { reason });
            self.show_source_cursor();
            let restore = self.local_restore();
            self.virtual_local_cursor = if self.virtual_edge_required() {
                self.local_bounds.center()
            } else {
                Point {
                    x: f64::from(restore.x),
                    y: f64::from(restore.y),
                }
            };
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

    fn remote_cursor_at_return_edge(edge: Edge, cursor: Point, remote: Size) -> bool {
        match edge {
            Edge::Left => {
                cursor.x >= f64::from(remote.width.saturating_sub(1)) - REMOTE_RETURN_MARGIN
            }
            Edge::Right => cursor.x <= REMOTE_RETURN_MARGIN,
            Edge::Top => {
                cursor.y >= f64::from(remote.height.saturating_sub(1)) - REMOTE_RETURN_MARGIN
            }
            Edge::Bottom => cursor.y <= REMOTE_RETURN_MARGIN,
        }
    }

    struct CaptureStats {
        active: AtomicBool,
        suspended: AtomicBool,
        mouse_hook_events: AtomicU64,
        keyboard_hook_events: AtomicU64,
        raw_input_repairs: AtomicU64,
        mouse_hook_repairs: AtomicU64,
        keyboard_hook_repairs: AtomicU64,
        input_pipeline_restarts: AtomicU64,
        callback_contention_drops: AtomicU64,
        input_supervisor_checks: AtomicU64,
        input_events: AtomicU64,
        control_events: AtomicU64,
        enter_events: AtomicU64,
        release_events: AtomicU64,
        return_edge_hits: AtomicU64,
        game_guard_blocks: AtomicU64,
        game_guard_releases: AtomicU64,
        suspend_toggles: AtomicU64,
        suspend_blocks: AtomicU64,
        suspend_auto_resumes: AtomicU64,
        send_failures: AtomicU64,
        unmapped_keys: AtomicU64,
    }

    impl CaptureStats {
        const fn new() -> Self {
            Self {
                active: AtomicBool::new(false),
                suspended: AtomicBool::new(false),
                mouse_hook_events: AtomicU64::new(0),
                keyboard_hook_events: AtomicU64::new(0),
                raw_input_repairs: AtomicU64::new(0),
                mouse_hook_repairs: AtomicU64::new(0),
                keyboard_hook_repairs: AtomicU64::new(0),
                input_pipeline_restarts: AtomicU64::new(0),
                callback_contention_drops: AtomicU64::new(0),
                input_supervisor_checks: AtomicU64::new(0),
                input_events: AtomicU64::new(0),
                control_events: AtomicU64::new(0),
                enter_events: AtomicU64::new(0),
                release_events: AtomicU64::new(0),
                return_edge_hits: AtomicU64::new(0),
                game_guard_blocks: AtomicU64::new(0),
                game_guard_releases: AtomicU64::new(0),
                suspend_toggles: AtomicU64::new(0),
                suspend_blocks: AtomicU64::new(0),
                suspend_auto_resumes: AtomicU64::new(0),
                send_failures: AtomicU64::new(0),
                unmapped_keys: AtomicU64::new(0),
            }
        }

        fn snapshot(&self) -> CaptureStatsSnapshot {
            CaptureStatsSnapshot {
                active: self.active.load(Ordering::Relaxed),
                suspended: self.suspended.load(Ordering::Relaxed),
                mouse_hook_installed: mouse_hook_installed(),
                mouse_hook_events: self.mouse_hook_events.load(Ordering::Relaxed),
                keyboard_hook_events: self.keyboard_hook_events.load(Ordering::Relaxed),
                raw_mouse_events: RAW_MOUSE_EVENTS.load(Ordering::Relaxed),
                raw_keyboard_events: RAW_KEYBOARD_EVENTS.load(Ordering::Relaxed),
                raw_input_repairs: self.raw_input_repairs.load(Ordering::Relaxed),
                mouse_hook_repairs: self.mouse_hook_repairs.load(Ordering::Relaxed),
                keyboard_hook_repairs: self.keyboard_hook_repairs.load(Ordering::Relaxed),
                input_pipeline_restarts: self.input_pipeline_restarts.load(Ordering::Relaxed),
                callback_contention_drops: self.callback_contention_drops.load(Ordering::Relaxed),
                input_supervisor_checks: self.input_supervisor_checks.load(Ordering::Relaxed),
                system_last_input_tick: system_last_input_tick().unwrap_or_default(),
                raw_worker_thread_id: RAW_WORKER_THREAD_ID.load(Ordering::Relaxed),
                hook_worker_thread_id: HOOK_WORKER_THREAD_ID.load(Ordering::Relaxed),
                input_events: self.input_events.load(Ordering::Relaxed),
                control_events: self.control_events.load(Ordering::Relaxed),
                enter_events: self.enter_events.load(Ordering::Relaxed),
                release_events: self.release_events.load(Ordering::Relaxed),
                return_edge_hits: self.return_edge_hits.load(Ordering::Relaxed),
                game_guard_blocks: self.game_guard_blocks.load(Ordering::Relaxed),
                game_guard_releases: self.game_guard_releases.load(Ordering::Relaxed),
                suspend_toggles: self.suspend_toggles.load(Ordering::Relaxed),
                suspend_blocks: self.suspend_blocks.load(Ordering::Relaxed),
                suspend_auto_resumes: self.suspend_auto_resumes.load(Ordering::Relaxed),
                send_failures: self.send_failures.load(Ordering::Relaxed),
                unmapped_keys: self.unmapped_keys.load(Ordering::Relaxed),
            }
        }
    }

    struct GameGuard {
        last_check: Instant,
        blocks_capture: bool,
        fullscreen: bool,
        cursor_confined: bool,
    }

    impl Default for GameGuard {
        fn default() -> Self {
            Self {
                last_check: Instant::now() - GAME_GUARD_CHECK_INTERVAL,
                blocks_capture: false,
                fullscreen: false,
                cursor_confined: false,
            }
        }
    }

    impl GameGuard {
        fn blocks_capture(&mut self, mode: GameCompatibilityMode) -> bool {
            self.refresh(mode);
            self.blocks_capture
        }

        fn virtual_edge_required(&mut self, mode: GameCompatibilityMode) -> bool {
            self.refresh(mode);
            mode != GameCompatibilityMode::Compatible && (self.fullscreen || self.cursor_confined)
        }

        fn refresh(&mut self, mode: GameCompatibilityMode) {
            if self.last_check.elapsed() < GAME_GUARD_CHECK_INTERVAL {
                return;
            }

            self.last_check = Instant::now();
            self.fullscreen = foreground_is_fullscreen();
            self.cursor_confined = cursor_is_confined();
            self.blocks_capture =
                game_guard_should_block(mode, self.fullscreen, self.cursor_confined);
        }
    }

    fn game_guard_should_block(
        mode: GameCompatibilityMode,
        fullscreen: bool,
        cursor_confined: bool,
    ) -> bool {
        match mode {
            GameCompatibilityMode::Compatible => fullscreen || cursor_confined,
            GameCompatibilityMode::Borderless => cursor_confined,
            GameCompatibilityMode::AlwaysEnabled => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            InputSupervisorSnapshot, full_pipeline_stalled, game_guard_should_block,
            hook_needs_repair, raw_input_stalled, remote_cursor_at_return_edge,
        };
        use edge_common::GameCompatibilityMode;
        use edge_geometry::{Point, Size};
        use edge_protocol::Edge;

        #[test]
        fn game_guard_modes_apply_expected_policy() {
            assert!(game_guard_should_block(
                GameCompatibilityMode::Compatible,
                true,
                false
            ));
            assert!(!game_guard_should_block(
                GameCompatibilityMode::Borderless,
                true,
                false
            ));
            assert!(game_guard_should_block(
                GameCompatibilityMode::Borderless,
                false,
                true
            ));
            assert!(!game_guard_should_block(
                GameCompatibilityMode::AlwaysEnabled,
                true,
                true
            ));
        }

        #[test]
        fn watchdog_only_repairs_when_raw_input_outpaces_a_hook() {
            assert!(hook_needs_repair(11, 10, 4, 4, true));
            assert!(!hook_needs_repair(10, 10, 4, 4, true));
            assert!(!hook_needs_repair(11, 10, 5, 4, true));
            assert!(!hook_needs_repair(11, 10, 4, 4, false));
        }

        #[test]
        fn supervisor_detects_system_input_missing_from_both_workers() {
            let previous = input_snapshot(10, 20, 30, 40, 100);
            let stalled = input_snapshot(10, 20, 30, 40, 101);
            let healthy = input_snapshot(11, 20, 31, 40, 102);

            assert!(full_pipeline_stalled(stalled, previous));
            assert!(!full_pipeline_stalled(healthy, stalled));
        }

        #[test]
        fn supervisor_detects_raw_input_worker_lag() {
            let previous = input_snapshot(10, 20, 30, 40, 100);
            let hook_only = input_snapshot(10, 20, 31, 40, 101);
            let both = input_snapshot(11, 20, 32, 40, 102);

            assert!(raw_input_stalled(hook_only, previous));
            assert!(!raw_input_stalled(both, hook_only));
        }

        fn input_snapshot(
            raw_mouse: u64,
            raw_keyboard: u64,
            hook_mouse: u64,
            hook_keyboard: u64,
            system_last_input: u32,
        ) -> InputSupervisorSnapshot {
            InputSupervisorSnapshot {
                raw_mouse,
                raw_keyboard,
                hook_mouse,
                hook_keyboard,
                system_last_input: Some(system_last_input),
            }
        }

        #[test]
        fn controller_validates_the_remote_return_edge() {
            let size = Size {
                width: 1920,
                height: 1080,
            };
            assert!(!remote_cursor_at_return_edge(
                Edge::Left,
                Point {
                    x: 1887.0,
                    y: 500.0
                },
                size
            ));
            assert!(remote_cursor_at_return_edge(
                Edge::Left,
                Point {
                    x: 1910.0,
                    y: 500.0
                },
                size
            ));
            assert!(remote_cursor_at_return_edge(
                Edge::Right,
                Point { x: 8.0, y: 500.0 },
                size
            ));
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

        fn center(&self) -> Point {
            Point {
                x: f64::from(self.left + self.width / 2),
                y: f64::from(self.top + self.height / 2),
            }
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

    fn foreground_is_fullscreen() -> bool {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return false;
            }

            let mut window = RECT::default();
            if GetWindowRect(hwnd, &mut window) == 0 {
                return false;
            }

            let monitor: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() {
                return false;
            }

            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                rcMonitor: RECT::default(),
                rcWork: RECT::default(),
                dwFlags: 0,
            };
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return false;
            }

            rect_covers(&window, &info.rcMonitor, FULLSCREEN_TOLERANCE_PX)
        }
    }

    fn foreground_window_id() -> isize {
        unsafe { GetForegroundWindow() as isize }
    }

    fn cursor_is_confined() -> bool {
        unsafe {
            let mut clip = RECT::default();
            if GetClipCursor(&mut clip) == 0 {
                return false;
            }

            let desktop = RECT {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
            };

            !rect_covers(&clip, &desktop, FULLSCREEN_TOLERANCE_PX)
        }
    }

    fn rect_covers(rect: &RECT, bounds: &RECT, tolerance: i32) -> bool {
        rect.left <= bounds.left + tolerance
            && rect.top <= bounds.top + tolerance
            && rect.right >= bounds.right - tolerance
            && rect.bottom >= bounds.bottom - tolerance
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
    use std::{
        ffi::c_void,
        mem::size_of,
        ptr::null_mut,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
    };

    use windows_sys::Win32::{
        Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CW_USEDEFAULT, CreateIcon, CreatePopupMenu, CreateWindowExW,
                DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW,
                GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_DISABLED, MF_SEPARATOR,
                MF_STRING, MSG, PostQuitMessage, RegisterClassW, SetForegroundWindow,
                TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
                TranslateMessage, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK,
                WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use crate::{Result, WindowsInputError, WindowsTrayCommand};

    const TRAY_ID: u32 = 1;
    const WM_TRAY_ICON: u32 = WM_APP + 1;
    const ID_SETTINGS: usize = 1001;
    const ID_RELEASE: usize = 1002;
    const ID_QUIT: usize = 1003;
    const ID_AUDIO: usize = 1004;

    static TRAY_STATUS: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    static TRAY_COMMANDS: Mutex<Option<mpsc::Sender<WindowsTrayCommand>>> = Mutex::new(None);
    static TRAY_HWND: AtomicUsize = AtomicUsize::new(0);
    static TRAY_ICON_HANDLE: AtomicUsize = AtomicUsize::new(0);

    pub fn run(status: &str, commands: mpsc::Sender<WindowsTrayCommand>) -> Result<()> {
        unsafe {
            set_tray_status(status);
            set_tray_commands(commands);

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

    pub fn update_status(status: &str) -> Result<()> {
        set_tray_status(status);
        let hwnd = TRAY_HWND.load(Ordering::Relaxed);
        if hwnd == 0 {
            return Ok(());
        }
        modify_tray_icon(hwnd as _, status)
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
                    let event = tray_notification_event(lparam);
                    if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK || event == NIN_SELECT {
                        send_tray_command(WindowsTrayCommand::OpenSettings);
                    } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                        show_menu(hwnd);
                    }
                    0
                }
                WM_COMMAND => {
                    dispatch_menu_command(hwnd, wparam & 0xffff);
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

    fn tray_notification_event(lparam: LPARAM) -> u32 {
        (lparam as u32) & 0xffff
    }

    unsafe fn dispatch_menu_command(hwnd: HWND, command: usize) {
        match command {
            ID_SETTINGS => send_tray_command(WindowsTrayCommand::OpenSettings),
            ID_RELEASE => {
                tracing::info!("release requested from tray");
                send_tray_command(WindowsTrayCommand::ReleaseControl);
            }
            ID_AUDIO => send_tray_command(WindowsTrayCommand::ToggleAudio),
            ID_QUIT => {
                send_tray_command(WindowsTrayCommand::Quit);
                remove_tray_icon(hwnd);
                unsafe { DestroyWindow(hwnd) };
            }
            _ => {}
        }
    }

    #[cfg(test)]
    mod tests {
        use super::tray_notification_event;

        #[test]
        fn extracts_version_four_notification_from_low_word() {
            let encoded = ((1_u32 << 16) | 0x0205) as isize;
            assert_eq!(tray_notification_event(encoded), 0x0205);
        }
    }

    fn add_tray_icon(hwnd: HWND, status: &str) -> Result<()> {
        let mut data = notify_icon_data(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY_ICON;
        let icon = create_mouse_icon(icon_color(status)).ok_or_else(|| {
            WindowsInputError::Tray("failed to create edge-kvm tray icon".to_string())
        })?;
        data.hIcon = icon;
        copy_wide("edge-kvm", status, &mut data.szTip);

        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            let error = unsafe { GetLastError() };
            destroy_icon(icon);
            return Err(WindowsInputError::Tray(format!(
                "Shell_NotifyIconW(NIM_ADD) failed with Win32 error {error}"
            )));
        }
        store_tray_icon(icon);
        TRAY_HWND.store(hwnd as usize, Ordering::Relaxed);

        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        if unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) } == 0 {
            let error = unsafe { GetLastError() };
            remove_tray_icon(hwnd);
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
        TRAY_HWND.store(0, Ordering::Relaxed);
        let icon = TRAY_ICON_HANDLE.swap(0, Ordering::Relaxed);
        if icon != 0 {
            destroy_icon(icon as _);
        }
    }

    fn modify_tray_icon(hwnd: HWND, status: &str) -> Result<()> {
        let icon = create_mouse_icon(icon_color(status)).ok_or_else(|| {
            WindowsInputError::Tray("failed to create edge-kvm tray icon".to_string())
        })?;
        let mut data = notify_icon_data(hwnd);
        data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.hIcon = icon;
        copy_wide("edge-kvm", status, &mut data.szTip);

        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
            let error = unsafe { GetLastError() };
            destroy_icon(icon);
            return Err(WindowsInputError::Tray(format!(
                "Shell_NotifyIconW(NIM_MODIFY) failed with Win32 error {error}"
            )));
        }
        store_tray_icon(icon);
        Ok(())
    }

    fn show_menu(hwnd: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }

        let status = current_tray_status();
        let settings = to_wide("Settings...");
        let release = to_wide("Release control");
        let audio = to_wide("Toggle Linux audio");
        let quit = to_wide("Quit");

        unsafe {
            AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, status.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
            AppendMenuW(menu, MF_STRING, ID_SETTINGS, settings.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_RELEASE, release.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_AUDIO, audio.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());

            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 {
                SetForegroundWindow(hwnd);
                let command = TrackPopupMenu(
                    menu,
                    TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null_mut(),
                );
                if command != 0 {
                    dispatch_menu_command(hwnd, command as usize);
                }
            }

            DestroyMenu(menu);
        }
    }

    fn set_tray_status(status: &str) {
        let mut tray_status = TRAY_STATUS.lock().expect("tray status poisoned");
        *tray_status = to_wide(status);
    }

    fn set_tray_commands(commands: mpsc::Sender<WindowsTrayCommand>) {
        let mut tray_commands = TRAY_COMMANDS.lock().expect("tray commands poisoned");
        *tray_commands = Some(commands);
    }

    fn send_tray_command(command: WindowsTrayCommand) {
        let tray_commands = TRAY_COMMANDS.lock().expect("tray commands poisoned");
        if let Some(commands) = tray_commands.as_ref() {
            let _ = commands.send(command);
        }
    }

    fn current_tray_status() -> Vec<u16> {
        let tray_status = TRAY_STATUS.lock().expect("tray status poisoned");
        if tray_status.is_empty() {
            to_wide("edge-kvm")
        } else {
            tray_status.clone()
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

    #[derive(Clone, Copy)]
    enum IconColor {
        Connecting,
        Connected,
        Disconnected,
    }

    fn icon_color(status: &str) -> IconColor {
        if status.starts_with("Connected") {
            IconColor::Connected
        } else if status.starts_with("Disconnected") {
            IconColor::Disconnected
        } else {
            IconColor::Connecting
        }
    }

    fn create_mouse_icon(color: IconColor) -> Option<*mut c_void> {
        const SIZE: i32 = 32;
        let mut xor_plane = vec![0_u8; (SIZE * SIZE * 4) as usize];
        let and_plane = vec![0_u8; ((SIZE * SIZE + 7) / 8) as usize];
        let fill = match color {
            IconColor::Connecting => [0x9c, 0xa3, 0xaf],
            IconColor::Connected => [0x22, 0xc5, 0x5e],
            IconColor::Disconnected => [0xef, 0x44, 0x44],
        };
        let outline = [0x11, 0x18, 0x27];
        let highlight = [0xff, 0xff, 0xff];

        for y in 0..SIZE {
            for x in 0..SIZE {
                let nx = (f64::from(x) + 0.5) / f64::from(SIZE);
                let ny = (f64::from(y) + 0.5) / f64::from(SIZE);
                let idx = ((y * SIZE + x) * 4) as usize;

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

                xor_plane[idx] = rgb[2];
                xor_plane[idx + 1] = rgb[1];
                xor_plane[idx + 2] = rgb[0];
                xor_plane[idx + 3] = alpha;
            }
        }

        let icon = unsafe {
            CreateIcon(
                null_mut(),
                SIZE,
                SIZE,
                1,
                32,
                and_plane.as_ptr(),
                xor_plane.as_ptr(),
            )
        };
        (!icon.is_null()).then_some(icon)
    }

    fn store_tray_icon(icon: *mut c_void) {
        let old_icon = TRAY_ICON_HANDLE.swap(icon as usize, Ordering::Relaxed);
        if old_icon != 0 {
            destroy_icon(old_icon as _);
        }
    }

    fn destroy_icon(icon: *mut c_void) {
        unsafe {
            DestroyIcon(icon);
        }
    }

    fn ellipse(x: f64, y: f64, cx: f64, cy: f64, rx: f64, ry: f64) -> bool {
        let dx = (x - cx) / rx;
        let dy = (y - cy) / ry;
        dx * dx + dy * dy <= 1.0
    }
}
