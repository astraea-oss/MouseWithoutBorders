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
mod capture {
    use std::{
        ptr::null_mut,
        sync::{Mutex, OnceLock, mpsc},
        thread,
    };

    use edge_protocol::Edge;
    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK,
            KBDLLHOOKSTRUCT, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SetCursorPos, SetWindowsHookExW,
            TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
            WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    };

    use crate::{
        CaptureConfig, CapturedInput, ControlEvent, InputEvent, Result, WindowsInputError, map_key,
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
    const XBUTTON1: u16 = 0x0001;
    const XBUTTON2: u16 = 0x0002;
    const WHEEL_DELTA: f64 = 120.0;

    static STATE: OnceLock<Mutex<CaptureState>> = OnceLock::new();

    pub fn start(config: CaptureConfig) -> Result<mpsc::Receiver<CapturedInput>> {
        let (sender, receiver) = mpsc::channel();
        let state = CaptureState {
            sender,
            config,
            local_bounds: LocalBounds::query(),
            active: false,
            anchor: POINT { x: 0, y: 0 },
            remote_cursor: Point { x: 0.0, y: 0.0 },
            ctrl_down: false,
            alt_down: false,
        };

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
            WM_MOUSEMOVE => state.remote_motion(mouse.pt),
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

        if down && keyboard.vkCode == VK_PAUSE && state.ctrl_down && state.alt_down {
            state.release_to_local(ReleaseReason::Hotkey);
            return 1;
        }

        if down || up {
            let scan_code = keyboard.scanCode as u16;
            let extended = keyboard.flags & LLKHF_EXTENDED != 0;
            match map_key(scan_code, extended) {
                Ok(evdev_code) => state.send_input(InputEvent::Key { evdev_code, down }),
                Err(err) => tracing::debug!(%err, scan_code, extended, "ignoring unmapped key"),
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
            self.anchor = self.local_bounds.anchor_for(self.config.edge, point);
            self.remote_cursor = self.remote_start(point);
            self.send_control(ControlEvent::EnterRemote {
                edge: self.config.edge,
                normalized_y: self.normalized_perpendicular(point),
            });
            unsafe {
                SetCursorPos(self.anchor.x, self.anchor.y);
            }
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
            self.send_input(InputEvent::AllKeysUp);
            self.send_control(ControlEvent::ReleaseToLocal { reason });
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

        fn remote_start(&self, point: POINT) -> Point {
            let normalized = f64::from(self.normalized_perpendicular(point));
            let remote = self.config.remote_size;
            match self.config.edge {
                Edge::Left => Point {
                    x: f64::from(remote.width.saturating_sub(2)),
                    y: normalized * f64::from(remote.height.saturating_sub(1)),
                },
                Edge::Right => Point {
                    x: 1.0,
                    y: normalized * f64::from(remote.height.saturating_sub(1)),
                },
                Edge::Top => Point {
                    x: normalized * f64::from(remote.width.saturating_sub(1)),
                    y: f64::from(remote.height.saturating_sub(2)),
                },
                Edge::Bottom => Point {
                    x: normalized * f64::from(remote.width.saturating_sub(1)),
                    y: 1.0,
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
            let _ = self.sender.send(CapturedInput::Input(event));
        }

        fn send_control(&self, event: ControlEvent) {
            let _ = self.sender.send(CapturedInput::Control(event));
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
}

#[cfg(windows)]
mod tray {
    use std::{ffi::c_void, mem::size_of, ptr::null_mut, sync::OnceLock};

    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
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
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY_ICON;
        data.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        copy_wide("edge-kvm", status, &mut data.szTip);

        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return Err(WindowsInputError::Tray(
                "Shell_NotifyIconW(NIM_ADD) failed".to_string(),
            ));
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
