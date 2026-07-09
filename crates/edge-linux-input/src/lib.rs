use std::{collections::BTreeSet, process::Stdio, sync::Arc};

use edge_common::ClipboardConfig;
use edge_protocol::{InputEvent, MouseButton, OutputInfo, ScreenInfo};
use serde::Deserialize;
use std::sync::Mutex;
use tokio::{io::AsyncWriteExt, process::Command};

#[cfg(target_os = "linux")]
mod wayland_virtual_input;

pub const LIBEI_PKG_CONFIG: &str = "libei-1.0";
pub const LIBOEFFIS_PKG_CONFIG: &str = "liboeffis-1.0";

#[derive(Debug, thiserror::Error)]
pub enum LinuxInputError {
    #[error("{pkg_config} is not available through pkg-config")]
    LibeiUnavailable { pkg_config: &'static str },
    #[error("failed to initialize libei backend: {0}")]
    LibeiInit(String),
    #[error("libei backend is not connected")]
    LibeiNotConnected,
    #[error("libei backend is unavailable on this platform")]
    LibeiUnsupportedPlatform,
    #[error("libei backend lock was poisoned")]
    LibeiLockPoisoned,
    #[error("libei backend has no device for {0}")]
    LibeiMissingDevice(&'static str),
    #[error("failed to initialize Hyprland virtual input backend: {0}")]
    HyprlandVirtualInputInit(String),
    #[error("Hyprland virtual input backend is unavailable on this platform")]
    HyprlandVirtualInputUnsupportedPlatform,
    #[error("Hyprland virtual input backend lock was poisoned")]
    HyprlandVirtualInputLockPoisoned,
    #[error("command `{program}` failed: {message}")]
    CommandFailed { program: String, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("clipboard text exceeds configured max_bytes ({max_bytes})")]
    ClipboardTooLarge { max_bytes: usize },
}

pub type Result<T> = std::result::Result<T, LinuxInputError>;

#[cfg(target_os = "linux")]
pub use wayland_virtual_input::HyprlandVirtualInputBackend;

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct HyprlandVirtualInputBackend;

#[cfg(not(target_os = "linux"))]
impl HyprlandVirtualInputBackend {
    pub fn connect() -> Result<Self> {
        Err(LinuxInputError::HyprlandVirtualInputUnsupportedPlatform)
    }

    pub async fn inject(&self, _event: InputEvent) -> Result<()> {
        Err(LinuxInputError::HyprlandVirtualInputUnsupportedPlatform)
    }

    pub async fn all_keys_up(&self) -> Result<()> {
        Err(LinuxInputError::HyprlandVirtualInputUnsupportedPlatform)
    }
}

#[derive(Debug, Clone)]
pub struct LibeiBackend {
    available: bool,
    version: Option<String>,
    sender: Option<Arc<Mutex<LibeiSender>>>,
}

impl LibeiBackend {
    pub fn probe() -> Self {
        let version = std::process::Command::new("pkg-config")
            .arg("--modversion")
            .arg(LIBEI_PKG_CONFIG)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|version| !version.is_empty());

        Self {
            available: version.is_some(),
            version,
            sender: None,
        }
    }

    pub fn connect() -> Result<Self> {
        let probe = Self::probe();
        if !probe.available {
            return Err(LinuxInputError::LibeiUnavailable {
                pkg_config: LIBEI_PKG_CONFIG,
            });
        }

        Ok(Self {
            available: true,
            version: probe.version,
            sender: Some(Arc::new(Mutex::new(LibeiSender::connect()?))),
        })
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn pkg_config_name(&self) -> &'static str {
        LIBEI_PKG_CONFIG
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub async fn inject(&self, event: InputEvent) -> Result<()> {
        if !self.available {
            return Err(LinuxInputError::LibeiUnavailable {
                pkg_config: LIBEI_PKG_CONFIG,
            });
        }

        let sender = self
            .sender
            .as_ref()
            .ok_or(LinuxInputError::LibeiNotConnected)?;
        let mut sender = sender
            .lock()
            .map_err(|_| LinuxInputError::LibeiLockPoisoned)?;
        sender.inject(event)
    }

    pub async fn all_keys_up(&self) -> Result<()> {
        self.inject(InputEvent::AllKeysUp).await
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
struct LibeiSender;

#[cfg(not(target_os = "linux"))]
impl LibeiSender {
    fn connect() -> Result<Self> {
        Err(LinuxInputError::LibeiUnsupportedPlatform)
    }

    fn inject(&mut self, _event: InputEvent) -> Result<()> {
        Err(LinuxInputError::LibeiUnsupportedPlatform)
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct LibeiSender {
    oeffis: *mut ffi::Oeffis,
    ei: *mut ffi::Ei,
    pointer: Option<*mut ffi::EiDevice>,
    button: Option<*mut ffi::EiDevice>,
    scroll: Option<*mut ffi::EiDevice>,
    keyboard: Option<*mut ffi::EiDevice>,
    started: BTreeSet<usize>,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u32>,
    sequence: u32,
}

#[cfg(target_os = "linux")]
unsafe impl Send for LibeiSender {}

#[cfg(target_os = "linux")]
impl LibeiSender {
    fn connect() -> Result<Self> {
        let oeffis = OeffisContext::connect_to_eis()?;
        let eis_fd = oeffis.eis_fd()?;
        let mut sender = Self::connect_ei(oeffis, eis_fd)?;
        sender.wait_for_devices()?;
        Ok(sender)
    }

    fn connect_ei(oeffis_context: OeffisContext, eis_fd: libc::c_int) -> Result<Self> {
        let ei = unsafe { ffi::ei_new_sender(std::ptr::null_mut()) };
        if ei.is_null() {
            return Err(LinuxInputError::LibeiInit(
                "ei_new_sender returned null".to_string(),
            ));
        }

        let name = std::ffi::CString::new("edge-kvm receiver")
            .map_err(|err| LinuxInputError::LibeiInit(err.to_string()))?;
        unsafe {
            ffi::ei_configure_name(ei, name.as_ptr());
            let rc = ffi::ei_setup_backend_fd(ei, eis_fd);
            if rc < 0 {
                ffi::ei_unref(ei);
                return Err(LinuxInputError::LibeiInit(format!(
                    "ei_setup_backend_fd failed: {rc}"
                )));
            }
        }

        Ok(Self {
            oeffis: oeffis_context.into_raw(),
            ei,
            pointer: None,
            button: None,
            scroll: None,
            keyboard: None,
            started: BTreeSet::new(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            sequence: 1,
        })
    }

    fn wait_for_devices(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            self.dispatch_once(500)?;
            if self.pointer.is_some() || self.button.is_some() || self.keyboard.is_some() {
                tracing::info!(
                    pointer = self.pointer.is_some(),
                    button = self.button.is_some(),
                    scroll = self.scroll.is_some(),
                    keyboard = self.keyboard.is_some(),
                    "libei sender backend ready"
                );
                return Ok(());
            }
        }

        Err(LinuxInputError::LibeiInit(
            "timed out waiting for libei devices".to_string(),
        ))
    }

    fn inject(&mut self, event: InputEvent) -> Result<()> {
        self.dispatch_available()?;

        match event {
            InputEvent::PointerMotion { dx, dy } => {
                let device = self.device_for(self.pointer, "pointer")?;
                self.start(device);
                unsafe {
                    ffi::ei_device_pointer_motion(device, dx, dy);
                    ffi::ei_device_frame(device, ffi::ei_now(self.ei));
                }
                Ok(())
            }
            InputEvent::PointerButton { button, down } => {
                let code = linux_button_code(button);
                let device = self.device_for(self.button, "button")?;
                self.start(device);
                unsafe {
                    ffi::ei_device_button_button(device, code, down);
                    ffi::ei_device_frame(device, ffi::ei_now(self.ei));
                }
                if down {
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                }
                Ok(())
            }
            InputEvent::PointerWheel { x, y } => {
                let device = self.device_for(self.scroll, "scroll")?;
                self.start(device);
                unsafe {
                    ffi::ei_device_scroll_delta(device, x, y);
                    ffi::ei_device_frame(device, ffi::ei_now(self.ei));
                }
                Ok(())
            }
            InputEvent::Key { evdev_code, down } => {
                let device = self.device_for(self.keyboard, "keyboard")?;
                self.start(device);
                unsafe {
                    ffi::ei_device_keyboard_key(device, u32::from(evdev_code), down);
                    ffi::ei_device_frame(device, ffi::ei_now(self.ei));
                }
                if down {
                    self.pressed_keys.insert(evdev_code);
                } else {
                    self.pressed_keys.remove(&evdev_code);
                }
                Ok(())
            }
            InputEvent::AllKeysUp => self.all_keys_up(),
        }
    }

    fn all_keys_up(&mut self) -> Result<()> {
        if let Some(device) = self.keyboard {
            self.start(device);
            for key in std::mem::take(&mut self.pressed_keys) {
                unsafe {
                    ffi::ei_device_keyboard_key(device, u32::from(key), false);
                }
            }
            unsafe {
                ffi::ei_device_frame(device, ffi::ei_now(self.ei));
            }
        }

        if let Some(device) = self.button {
            self.start(device);
            for button in std::mem::take(&mut self.pressed_buttons) {
                unsafe {
                    ffi::ei_device_button_button(device, button, false);
                }
            }
            unsafe {
                ffi::ei_device_frame(device, ffi::ei_now(self.ei));
            }
        }

        Ok(())
    }

    fn dispatch_available(&mut self) -> Result<()> {
        loop {
            let fd = unsafe { ffi::ei_get_fd(self.ei) };
            if fd < 0 || !poll_fd(fd, 0)? {
                break;
            }
            self.dispatch_events();
        }
        Ok(())
    }

    fn dispatch_once(&mut self, timeout_ms: i32) -> Result<()> {
        let fd = unsafe { ffi::ei_get_fd(self.ei) };
        if fd < 0 {
            return Err(LinuxInputError::LibeiInit(
                "ei_get_fd returned an invalid fd".to_string(),
            ));
        }
        if poll_fd(fd, timeout_ms)? {
            self.dispatch_events();
        }
        Ok(())
    }

    fn dispatch_events(&mut self) {
        unsafe {
            ffi::ei_dispatch(self.ei);
        }

        loop {
            let event = unsafe { ffi::ei_get_event(self.ei) };
            if event.is_null() {
                break;
            }
            self.handle_event(event);
            unsafe {
                ffi::ei_event_unref(event);
            }
        }
    }

    fn handle_event(&mut self, event: *mut ffi::EiEvent) {
        let event_type = unsafe { ffi::ei_event_get_type(event) };
        match event_type {
            ffi::EI_EVENT_CONNECT => tracing::info!("libei connected"),
            ffi::EI_EVENT_DISCONNECT => tracing::warn!("libei disconnected"),
            ffi::EI_EVENT_SEAT_ADDED => {
                let seat = unsafe { ffi::ei_event_get_seat(event) };
                if !seat.is_null() {
                    unsafe {
                        ffi::ei_seat_bind_capabilities(
                            seat,
                            ffi::EI_DEVICE_CAP_POINTER,
                            ffi::EI_DEVICE_CAP_BUTTON,
                            ffi::EI_DEVICE_CAP_SCROLL,
                            ffi::EI_DEVICE_CAP_KEYBOARD,
                            0 as libc::c_uint,
                        );
                        ffi::ei_seat_request_device_with_capabilities(
                            seat,
                            ffi::EI_DEVICE_CAP_POINTER,
                            ffi::EI_DEVICE_CAP_BUTTON,
                            ffi::EI_DEVICE_CAP_SCROLL,
                            ffi::EI_DEVICE_CAP_KEYBOARD,
                            0 as libc::c_uint,
                        );
                    }
                    tracing::info!("requested libei pointer/button/scroll/keyboard capabilities");
                }
            }
            ffi::EI_EVENT_DEVICE_ADDED => {
                let device = unsafe { ffi::ei_event_get_device(event) };
                if !device.is_null() {
                    self.register_device(device);
                }
            }
            ffi::EI_EVENT_DEVICE_RESUMED => {
                let device = unsafe { ffi::ei_event_get_device(event) };
                if !device.is_null() {
                    self.start(device);
                }
            }
            ffi::EI_EVENT_DEVICE_PAUSED => {
                let device = unsafe { ffi::ei_event_get_device(event) };
                if !device.is_null() {
                    self.started.remove(&(device as usize));
                }
            }
            _ => {}
        }
    }

    fn register_device(&mut self, device: *mut ffi::EiDevice) {
        let name = unsafe {
            let ptr = ffi::ei_device_get_name(device);
            if ptr.is_null() {
                "(unnamed)".to_string()
            } else {
                std::ffi::CStr::from_ptr(ptr).to_string_lossy().to_string()
            }
        };

        unsafe {
            if self.pointer.is_none()
                && ffi::ei_device_has_capability(device, ffi::EI_DEVICE_CAP_POINTER)
            {
                self.pointer = Some(ffi::ei_device_ref(device));
            }
            if self.button.is_none()
                && ffi::ei_device_has_capability(device, ffi::EI_DEVICE_CAP_BUTTON)
            {
                self.button = Some(ffi::ei_device_ref(device));
            }
            if self.scroll.is_none()
                && ffi::ei_device_has_capability(device, ffi::EI_DEVICE_CAP_SCROLL)
            {
                self.scroll = Some(ffi::ei_device_ref(device));
            }
            if self.keyboard.is_none()
                && ffi::ei_device_has_capability(device, ffi::EI_DEVICE_CAP_KEYBOARD)
            {
                self.keyboard = Some(ffi::ei_device_ref(device));
            }
        }

        tracing::info!(%name, "libei device added");
    }

    fn start(&mut self, device: *mut ffi::EiDevice) {
        if self.started.insert(device as usize) {
            let sequence = self.sequence;
            self.sequence = self.sequence.wrapping_add(1).max(1);
            unsafe {
                ffi::ei_device_start_emulating(device, sequence);
            }
        }
    }

    fn device_for(
        &mut self,
        device: Option<*mut ffi::EiDevice>,
        capability: &'static str,
    ) -> Result<*mut ffi::EiDevice> {
        if let Some(device) = device {
            return Ok(device);
        }
        self.wait_for_devices()?;
        device.ok_or(LinuxInputError::LibeiMissingDevice(capability))
    }
}

#[cfg(target_os = "linux")]
impl Drop for LibeiSender {
    fn drop(&mut self) {
        let _ = self.all_keys_up();
        unsafe {
            if let Some(device) = self.pointer.take() {
                ffi::ei_device_unref(device);
            }
            if let Some(device) = self.button.take() {
                ffi::ei_device_unref(device);
            }
            if let Some(device) = self.scroll.take() {
                ffi::ei_device_unref(device);
            }
            if let Some(device) = self.keyboard.take() {
                ffi::ei_device_unref(device);
            }
            if !self.ei.is_null() {
                ffi::ei_disconnect(self.ei);
                ffi::ei_unref(self.ei);
            }
            if !self.oeffis.is_null() {
                ffi::oeffis_unref(self.oeffis);
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct OeffisContext {
    raw: *mut ffi::Oeffis,
}

#[cfg(target_os = "linux")]
impl OeffisContext {
    fn connect_to_eis() -> Result<Self> {
        let raw = unsafe { ffi::oeffis_new(std::ptr::null_mut()) };
        if raw.is_null() {
            return Err(LinuxInputError::LibeiInit(
                "oeffis_new returned null".to_string(),
            ));
        }

        unsafe {
            ffi::oeffis_create_session(
                raw,
                ffi::OEFFIS_DEVICE_POINTER | ffi::OEFFIS_DEVICE_KEYBOARD,
            );
        }

        let context = Self { raw };
        context.wait_connected()?;
        Ok(context)
    }

    fn wait_connected(&self) -> Result<()> {
        let fd = unsafe { ffi::oeffis_get_fd(self.raw) };
        if fd < 0 {
            return Err(LinuxInputError::LibeiInit(
                "oeffis_get_fd returned an invalid fd".to_string(),
            ));
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if poll_fd(fd, 500)? {
                unsafe {
                    ffi::oeffis_dispatch(self.raw);
                }
                loop {
                    let event = unsafe { ffi::oeffis_get_event(self.raw) };
                    match event {
                        ffi::OEFFIS_EVENT_NONE => break,
                        ffi::OEFFIS_EVENT_CONNECTED_TO_EIS => return Ok(()),
                        ffi::OEFFIS_EVENT_CLOSED => {
                            return Err(LinuxInputError::LibeiInit(
                                "RemoteDesktop portal session was closed".to_string(),
                            ));
                        }
                        ffi::OEFFIS_EVENT_DISCONNECTED => {
                            return Err(LinuxInputError::LibeiInit(self.error_message()));
                        }
                        other => tracing::debug!(event = other, "unknown oeffis event"),
                    }
                }
            }
        }

        Err(LinuxInputError::LibeiInit(
            "timed out waiting for RemoteDesktop portal EIS fd".to_string(),
        ))
    }

    fn eis_fd(&self) -> Result<libc::c_int> {
        let fd = unsafe { ffi::oeffis_get_eis_fd(self.raw) };
        if fd < 0 {
            return Err(LinuxInputError::LibeiInit(
                "oeffis_get_eis_fd failed".to_string(),
            ));
        }
        Ok(fd)
    }

    fn error_message(&self) -> String {
        unsafe {
            let ptr = ffi::oeffis_get_error_message(self.raw);
            if ptr.is_null() {
                "RemoteDesktop portal disconnected".to_string()
            } else {
                std::ffi::CStr::from_ptr(ptr).to_string_lossy().to_string()
            }
        }
    }

    fn into_raw(mut self) -> *mut ffi::Oeffis {
        let raw = self.raw;
        self.raw = std::ptr::null_mut();
        raw
    }
}

#[cfg(target_os = "linux")]
impl Drop for OeffisContext {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                ffi::oeffis_unref(self.raw);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn poll_fd(fd: libc::c_int, timeout_ms: i32) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if rc < 0 {
        return Err(LinuxInputError::Io(std::io::Error::last_os_error()));
    }
    Ok(rc > 0 && (pollfd.revents & libc::POLLIN) != 0)
}

#[cfg(target_os = "linux")]
fn linux_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
    }
}

#[cfg(target_os = "linux")]
#[allow(non_upper_case_globals)]
mod ffi {
    use libc::{c_char, c_int, c_uint, c_void, size_t};

    pub enum Ei {}
    pub enum EiEvent {}
    pub enum EiSeat {}
    pub enum EiDevice {}
    pub enum Oeffis {}

    pub const EI_DEVICE_CAP_POINTER: c_uint = 1 << 0;
    pub const EI_DEVICE_CAP_KEYBOARD: c_uint = 1 << 2;
    pub const EI_DEVICE_CAP_SCROLL: c_uint = 1 << 4;
    pub const EI_DEVICE_CAP_BUTTON: c_uint = 1 << 5;

    pub const EI_EVENT_CONNECT: c_uint = 1;
    pub const EI_EVENT_DISCONNECT: c_uint = 2;
    pub const EI_EVENT_SEAT_ADDED: c_uint = 3;
    pub const EI_EVENT_DEVICE_ADDED: c_uint = 5;
    pub const EI_EVENT_DEVICE_PAUSED: c_uint = 7;
    pub const EI_EVENT_DEVICE_RESUMED: c_uint = 8;

    pub const OEFFIS_DEVICE_KEYBOARD: c_uint = 1 << 0;
    pub const OEFFIS_DEVICE_POINTER: c_uint = 1 << 1;

    pub const OEFFIS_EVENT_NONE: c_uint = 0;
    pub const OEFFIS_EVENT_CONNECTED_TO_EIS: c_uint = 1;
    pub const OEFFIS_EVENT_CLOSED: c_uint = 2;
    pub const OEFFIS_EVENT_DISCONNECTED: c_uint = 3;

    #[link(name = "ei")]
    unsafe extern "C" {
        pub fn ei_new_sender(user_data: *mut c_void) -> *mut Ei;
        pub fn ei_unref(ei: *mut Ei) -> *mut Ei;
        pub fn ei_configure_name(ei: *mut Ei, name: *const c_char);
        pub fn ei_setup_backend_fd(ei: *mut Ei, fd: c_int) -> c_int;
        pub fn ei_get_fd(ei: *mut Ei) -> c_int;
        pub fn ei_dispatch(ei: *mut Ei);
        pub fn ei_get_event(ei: *mut Ei) -> *mut EiEvent;
        pub fn ei_now(ei: *mut Ei) -> u64;
        pub fn ei_disconnect(ei: *mut Ei);

        pub fn ei_event_unref(event: *mut EiEvent) -> *mut EiEvent;
        pub fn ei_event_get_type(event: *mut EiEvent) -> c_uint;
        pub fn ei_event_get_device(event: *mut EiEvent) -> *mut EiDevice;
        pub fn ei_event_get_seat(event: *mut EiEvent) -> *mut EiSeat;

        pub fn ei_seat_bind_capabilities(seat: *mut EiSeat, ...);
        pub fn ei_seat_request_device_with_capabilities(seat: *mut EiSeat, ...);

        pub fn ei_device_ref(device: *mut EiDevice) -> *mut EiDevice;
        pub fn ei_device_unref(device: *mut EiDevice) -> *mut EiDevice;
        pub fn ei_device_get_name(device: *mut EiDevice) -> *const c_char;
        pub fn ei_device_has_capability(device: *mut EiDevice, cap: c_uint) -> bool;
        pub fn ei_device_start_emulating(device: *mut EiDevice, sequence: u32);
        pub fn ei_device_pointer_motion(device: *mut EiDevice, x: f64, y: f64);
        pub fn ei_device_button_button(device: *mut EiDevice, button: u32, is_press: bool);
        pub fn ei_device_scroll_delta(device: *mut EiDevice, x: f64, y: f64);
        pub fn ei_device_keyboard_key(device: *mut EiDevice, keycode: u32, is_press: bool);
        pub fn ei_device_frame(device: *mut EiDevice, time: u64);
    }

    #[link(name = "oeffis")]
    unsafe extern "C" {
        pub fn oeffis_new(user_data: *mut c_void) -> *mut Oeffis;
        pub fn oeffis_unref(oeffis: *mut Oeffis) -> *mut Oeffis;
        pub fn oeffis_get_fd(oeffis: *mut Oeffis) -> c_int;
        pub fn oeffis_get_eis_fd(oeffis: *mut Oeffis) -> c_int;
        pub fn oeffis_create_session(oeffis: *mut Oeffis, devices: c_uint);
        pub fn oeffis_dispatch(oeffis: *mut Oeffis);
        pub fn oeffis_get_event(oeffis: *mut Oeffis) -> c_uint;
        pub fn oeffis_get_error_message(oeffis: *mut Oeffis) -> *const c_char;
    }

    #[allow(dead_code)]
    type _KeepSizeT = size_t;
}

pub async fn hyprland_screen_info(primary: &str) -> Result<ScreenInfo> {
    let output = Command::new("hyprctl")
        .arg("monitors")
        .arg("-j")
        .output()
        .await?;
    if !output.status.success() {
        return Err(LinuxInputError::CommandFailed {
            program: "hyprctl".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let monitors: Vec<HyprMonitor> = serde_json::from_slice(&output.stdout)?;
    let outputs = monitors
        .into_iter()
        .map(|monitor| OutputInfo {
            name: monitor.name,
            width: monitor.width,
            height: monitor.height,
            scale: monitor.scale,
            x: monitor.x,
            y: monitor.y,
        })
        .collect();

    Ok(ScreenInfo {
        outputs,
        primary_output: primary.to_string(),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct HyprCursorPosition {
    pub x: i32,
    pub y: i32,
}

pub async fn hyprland_cursor_position() -> Result<HyprCursorPosition> {
    let output = Command::new("hyprctl").arg("cursorpos").output().await?;
    if !output.status.success() {
        return Err(LinuxInputError::CommandFailed {
            program: "hyprctl cursorpos".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_hypr_cursor_position(text.trim()).ok_or_else(|| LinuxInputError::CommandFailed {
        program: "hyprctl cursorpos".to_string(),
        message: format!("unexpected output: {text:?}"),
    })
}

fn parse_hypr_cursor_position(text: &str) -> Option<HyprCursorPosition> {
    let (x, y) = text.split_once(',')?;
    Some(HyprCursorPosition {
        x: x.trim().parse().ok()?,
        y: y.trim().parse().ok()?,
    })
}

pub async fn read_clipboard_text(config: &ClipboardConfig) -> Result<Option<String>> {
    if !config.enabled {
        return Ok(None);
    }

    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .arg("--type")
        .arg("text")
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    if output.stdout.len() > config.max_bytes {
        return Err(LinuxInputError::ClipboardTooLarge {
            max_bytes: config.max_bytes,
        });
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

pub async fn write_clipboard_text(config: &ClipboardConfig, text: &str) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    if text.len() > config.max_bytes {
        return Err(LinuxInputError::ClipboardTooLarge {
            max_bytes: config.max_bytes,
        });
    }

    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = &mut child.stdin {
        stdin.write_all(text.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(LinuxInputError::CommandFailed {
            program: "wl-copy".to_string(),
            message: format!("exited with {status}"),
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HyprMonitor {
    name: String,
    width: u32,
    height: u32,
    #[serde(default)]
    scale: f32,
    x: i32,
    y: i32,
}
