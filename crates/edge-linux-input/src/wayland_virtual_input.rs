use std::{
    collections::BTreeSet,
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::fd::{AsFd, FromRawFd},
    sync::{Arc, Mutex},
    time::Instant,
};

use edge_protocol::{InputEvent, MouseButton};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::{LinuxInputError, Result};

mod virtual_keyboard {
    #![allow(clippy::all)]
    #![allow(missing_docs)]

    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
    }

    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
}

use virtual_keyboard::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const XKB_KEYMAP: &str = r#"xkb_keymap {
    xkb_keycodes  { include "evdev+aliases(qwerty)" };
    xkb_types     { include "complete" };
    xkb_compat    { include "complete" };
    xkb_symbols   { include "pc+us+inet(evdev)" };
};
"#;
const WHEEL_AXIS_STEP: f64 = 15.0;

#[derive(Debug, Clone)]
pub struct HyprlandVirtualInputBackend {
    sender: Arc<Mutex<HyprlandVirtualInput>>,
}

impl HyprlandVirtualInputBackend {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            sender: Arc::new(Mutex::new(HyprlandVirtualInput::connect()?)),
        })
    }

    pub async fn inject(&self, event: InputEvent) -> Result<()> {
        let mut sender = self
            .sender
            .lock()
            .map_err(|_| LinuxInputError::HyprlandVirtualInputLockPoisoned)?;
        sender.inject(event)
    }

    pub async fn all_keys_up(&self) -> Result<()> {
        self.inject(InputEvent::AllKeysUp).await
    }
}

#[derive(Debug)]
struct HyprlandVirtualInput {
    connection: Connection,
    event_queue: EventQueue<WaylandState>,
    state: WaylandState,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    _keymap: File,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u32>,
    modifiers: ModifierState,
    started_at: Instant,
}

unsafe impl Send for HyprlandVirtualInput {}

impl HyprlandVirtualInput {
    fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env()
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;
        let (globals, mut event_queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;
        let qh = event_queue.handle();
        let mut state = WaylandState;

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|err| missing_global("wl_seat", err))?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|err| missing_global("zwlr_virtual_pointer_manager_v1", err))?;
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|err| missing_global("zwp_virtual_keyboard_manager_v1", err))?;

        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        let keymap = create_keymap_file()?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1,
            keymap.as_fd(),
            keymap_size(),
        );

        event_queue
            .roundtrip(&mut state)
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;
        connection
            .flush()
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;

        tracing::info!("using Hyprland Wayland virtual input backend");

        Ok(Self {
            connection,
            event_queue,
            state,
            pointer,
            keyboard,
            _keymap: keymap,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            modifiers: ModifierState::default(),
            started_at: Instant::now(),
        })
    }

    fn inject(&mut self, event: InputEvent) -> Result<()> {
        self.dispatch_pending()?;

        match event {
            InputEvent::PointerMotion { dx, dy } => {
                self.pointer.motion(self.time_ms(), dx, dy);
                self.pointer.frame();
            }
            InputEvent::PointerButton { button, down } => {
                let code = linux_button_code(button);
                let state = if down {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                self.pointer.button(self.time_ms(), code, state);
                self.pointer.frame();
                if down {
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                }
            }
            InputEvent::PointerWheel { x, y } => {
                let time = self.time_ms();
                if x != 0.0 {
                    self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                    self.pointer.axis_discrete(
                        time,
                        wl_pointer::Axis::HorizontalScroll,
                        x * WHEEL_AXIS_STEP,
                        x.round() as i32,
                    );
                }
                if y != 0.0 {
                    let value = -y * WHEEL_AXIS_STEP;
                    self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                    self.pointer.axis_discrete(
                        time,
                        wl_pointer::Axis::VerticalScroll,
                        value,
                        value.signum() as i32,
                    );
                }
                self.pointer.frame();
            }
            InputEvent::Key { evdev_code, down } => {
                let state = if down {
                    wl_keyboard::KeyState::Pressed
                } else {
                    wl_keyboard::KeyState::Released
                };
                self.keyboard
                    .key(self.time_ms(), u32::from(evdev_code), state.into());
                let was_pressed = self.pressed_keys.contains(&evdev_code);
                if down {
                    self.pressed_keys.insert(evdev_code);
                } else {
                    self.pressed_keys.remove(&evdev_code);
                }
                if self.modifiers.update(evdev_code, down, was_pressed) {
                    self.keyboard.modifiers(
                        self.modifiers.depressed(),
                        0,
                        self.modifiers.locked(),
                        0,
                    );
                }
            }
            InputEvent::AllKeysUp => self.all_keys_up(),
        }

        self.connection
            .flush()
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))
    }

    fn all_keys_up(&mut self) {
        let time = self.time_ms();
        for key in std::mem::take(&mut self.pressed_keys) {
            self.keyboard
                .key(time, u32::from(key), wl_keyboard::KeyState::Released.into());
        }
        if self.modifiers.clear_depressed() {
            self.keyboard.modifiers(0, 0, self.modifiers.locked(), 0);
        }
        for button in std::mem::take(&mut self.pressed_buttons) {
            self.pointer
                .button(time, button, wl_pointer::ButtonState::Released);
        }
        self.pointer.frame();
    }

    fn dispatch_pending(&mut self) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;
        Ok(())
    }

    fn time_ms(&self) -> u32 {
        let elapsed = self.started_at.elapsed().as_millis();
        elapsed.min(u128::from(u32::MAX)) as u32
    }
}

#[derive(Debug, Default)]
struct ModifierState {
    shift: u8,
    control: u8,
    alt: u8,
    logo: u8,
    caps_lock: bool,
}

impl ModifierState {
    fn update(&mut self, evdev_code: u16, down: bool, was_pressed: bool) -> bool {
        if evdev_code == 58 {
            if down && !was_pressed {
                self.caps_lock = !self.caps_lock;
                return true;
            }
            return false;
        }

        if down && was_pressed {
            return false;
        }

        let counter = match evdev_code {
            42 | 54 => &mut self.shift,
            29 | 97 => &mut self.control,
            56 | 100 => &mut self.alt,
            125 | 126 => &mut self.logo,
            _ => return false,
        };

        let previous = *counter;
        if down {
            *counter = counter.saturating_add(1);
        } else {
            *counter = counter.saturating_sub(1);
        }

        (previous > 0) != (*counter > 0)
    }

    fn clear_depressed(&mut self) -> bool {
        let had_modifiers = self.depressed() != 0;
        self.shift = 0;
        self.control = 0;
        self.alt = 0;
        self.logo = 0;
        had_modifiers
    }

    fn depressed(&self) -> u32 {
        let mut mask = 0;
        if self.shift > 0 {
            mask |= 1 << 0;
        }
        if self.control > 0 {
            mask |= 1 << 2;
        }
        if self.alt > 0 {
            mask |= 1 << 3;
        }
        if self.logo > 0 {
            mask |= 1 << 6;
        }
        mask
    }

    fn locked(&self) -> u32 {
        if self.caps_lock { 1 << 1 } else { 0 }
    }
}

impl Drop for HyprlandVirtualInput {
    fn drop(&mut self) {
        self.all_keys_up();
        let _ = self.connection.flush();
        self.keyboard.destroy();
        self.pointer.destroy();
        let _ = self.connection.flush();
    }
}

#[derive(Debug)]
struct WaylandState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn create_keymap_file() -> Result<File> {
    let keymap = format!("{XKB_KEYMAP}\0");
    let name = std::ffi::CString::new("edge-kvm-keymap")
        .map_err(|err| LinuxInputError::HyprlandVirtualInputInit(err.to_string()))?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(LinuxInputError::Io(std::io::Error::last_os_error()));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(keymap.as_bytes())?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn keymap_size() -> u32 {
    (XKB_KEYMAP.len() + 1) as u32
}

fn missing_global(
    interface: &'static str,
    err: wayland_client::globals::BindError,
) -> LinuxInputError {
    LinuxInputError::HyprlandVirtualInputInit(format!("missing Wayland global {interface}: {err}"))
}

fn linux_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_control_modifier_mask() {
        let mut modifiers = ModifierState::default();

        assert!(modifiers.update(29, true, false));
        assert_eq!(modifiers.depressed(), 1 << 2);
        assert!(modifiers.update(29, false, true));
        assert_eq!(modifiers.depressed(), 0);
    }

    #[test]
    fn keeps_modifier_active_until_both_sides_release() {
        let mut modifiers = ModifierState::default();

        assert!(modifiers.update(29, true, false));
        assert!(!modifiers.update(97, true, false));
        assert_eq!(modifiers.depressed(), 1 << 2);
        assert!(!modifiers.update(29, false, true));
        assert_eq!(modifiers.depressed(), 1 << 2);
        assert!(modifiers.update(97, false, true));
        assert_eq!(modifiers.depressed(), 0);
    }

    #[test]
    fn tracks_common_modifier_masks() {
        let mut modifiers = ModifierState::default();

        assert!(modifiers.update(42, true, false));
        assert!(modifiers.update(56, true, false));
        assert!(modifiers.update(125, true, false));
        assert_eq!(modifiers.depressed(), (1 << 0) | (1 << 3) | (1 << 6));
        assert!(modifiers.clear_depressed());
        assert_eq!(modifiers.depressed(), 0);
    }

    #[test]
    fn toggles_caps_lock_locked_mask() {
        let mut modifiers = ModifierState::default();

        assert!(modifiers.update(58, true, false));
        assert_eq!(modifiers.locked(), 1 << 1);
        assert!(!modifiers.update(58, true, true));
        assert_eq!(modifiers.locked(), 1 << 1);
        assert!(!modifiers.update(58, false, true));
        assert_eq!(modifiers.locked(), 1 << 1);
        assert!(modifiers.update(58, true, false));
        assert_eq!(modifiers.locked(), 0);
    }

    #[test]
    fn clearing_depressed_modifiers_preserves_caps_lock() {
        let mut modifiers = ModifierState::default();

        assert!(modifiers.update(58, true, false));
        assert!(modifiers.update(29, true, false));
        assert!(modifiers.clear_depressed());
        assert_eq!(modifiers.depressed(), 0);
        assert_eq!(modifiers.locked(), 1 << 1);
    }
}
