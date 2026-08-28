use std::collections::BTreeSet;

use edge_protocol::{InputEvent, MouseButton};

use crate::{Result, WindowsInputError};

#[cfg(windows)]
pub(crate) const EDGE_KVM_INJECTION_TAG: usize = 0x4544_4b56;

#[cfg(windows)]
pub(crate) const fn is_edge_kvm_injected(extra_info: usize) -> bool {
    extra_info == EDGE_KVM_INJECTION_TAG
}
#[cfg(windows)]
const WHEEL_DELTA: f64 = 120.0;

#[derive(Default)]
pub struct WindowsInputInjector {
    held_keys: BTreeSet<u16>,
    held_buttons: [bool; 5],
}

impl WindowsInputInjector {
    pub fn new() -> Result<Self> {
        if cfg!(windows) {
            Ok(Self::default())
        } else {
            Err(WindowsInputError::UnsupportedPlatform)
        }
    }

    pub fn inject(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::PointerMotion { dx, dy } => {
                send_mouse(rounded_i32(dx), rounded_i32(dy), 0, mouse_move_flag())
            }
            InputEvent::PointerButton { button, down } => {
                let index = button_index(button);
                self.held_buttons[index] = down;
                let (data, flags) = mouse_button_input(button, down);
                send_mouse(0, 0, data, flags)
            }
            InputEvent::PointerWheel { x, y } => {
                if y != 0.0 {
                    send_mouse(0, 0, wheel_data(y), vertical_wheel_flag())?;
                }
                if x != 0.0 {
                    send_mouse(0, 0, wheel_data(x), horizontal_wheel_flag())?;
                }
                Ok(())
            }
            InputEvent::Key { evdev_code, down } => {
                if down {
                    self.held_keys.insert(evdev_code);
                } else {
                    self.held_keys.remove(&evdev_code);
                }
                send_key(evdev_code, down)
            }
            InputEvent::AllKeysUp => self.all_keys_up(),
        }
    }

    pub fn all_keys_up(&mut self) -> Result<()> {
        let keys: Vec<u16> = self.held_keys.iter().copied().collect();
        let mut first_error = None;
        for key in keys {
            if let Err(error) = send_key(key, false) {
                first_error.get_or_insert(error);
            }
        }
        self.held_keys.clear();

        for (index, held) in self.held_buttons.iter_mut().enumerate() {
            if !*held {
                continue;
            }
            let button = button_from_index(index);
            let (data, flags) = mouse_button_input(button, false);
            if let Err(error) = send_mouse(0, 0, data, flags) {
                first_error.get_or_insert(error);
            }
            *held = false;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for WindowsInputInjector {
    fn drop(&mut self) {
        let _ = self.all_keys_up();
    }
}

fn rounded_i32(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(windows)]
fn wheel_data(value: f64) -> u32 {
    rounded_i32(value * WHEEL_DELTA) as u32
}

#[cfg(not(windows))]
fn wheel_data(_value: f64) -> u32 {
    0
}

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

fn button_from_index(index: usize) -> MouseButton {
    match index {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => unreachable!("button index is internal and bounded"),
    }
}

#[cfg(windows)]
fn mouse_button_input(button: MouseButton, down: bool) -> (u32, u32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    };

    match (button, down) {
        (MouseButton::Left, true) => (0, MOUSEEVENTF_LEFTDOWN),
        (MouseButton::Left, false) => (0, MOUSEEVENTF_LEFTUP),
        (MouseButton::Right, true) => (0, MOUSEEVENTF_RIGHTDOWN),
        (MouseButton::Right, false) => (0, MOUSEEVENTF_RIGHTUP),
        (MouseButton::Middle, true) => (0, MOUSEEVENTF_MIDDLEDOWN),
        (MouseButton::Middle, false) => (0, MOUSEEVENTF_MIDDLEUP),
        (MouseButton::Back, true) => (1, MOUSEEVENTF_XDOWN),
        (MouseButton::Back, false) => (1, MOUSEEVENTF_XUP),
        (MouseButton::Forward, true) => (2, MOUSEEVENTF_XDOWN),
        (MouseButton::Forward, false) => (2, MOUSEEVENTF_XUP),
    }
}

#[cfg(not(windows))]
fn mouse_button_input(_button: MouseButton, _down: bool) -> (u32, u32) {
    (0, 0)
}

#[cfg(windows)]
fn mouse_move_flag() -> u32 {
    windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE
}

#[cfg(not(windows))]
fn mouse_move_flag() -> u32 {
    0
}

#[cfg(windows)]
fn vertical_wheel_flag() -> u32 {
    windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_WHEEL
}

#[cfg(not(windows))]
fn vertical_wheel_flag() -> u32 {
    0
}

#[cfg(windows)]
fn horizontal_wheel_flag() -> u32 {
    windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_HWHEEL
}

#[cfg(not(windows))]
fn horizontal_wheel_flag() -> u32 {
    0
}

#[cfg(windows)]
fn send_mouse(dx: i32, dy: i32, data: u32, flags: u32) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_MOUSE, MOUSEINPUT, SendInput,
    };

    let mut input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    input.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: EDGE_KVM_INJECTION_TAG,
    };
    send_inputs(&[input], SendInput)
}

#[cfg(not(windows))]
fn send_mouse(_dx: i32, _dy: i32, _data: u32, _flags: u32) -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
fn send_key(evdev_code: u16, down: bool) -> Result<()> {
    use edge_keymap::evdev_to_windows_scancode;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, SendInput,
    };

    let key = evdev_to_windows_scancode(evdev_code)
        .ok_or(WindowsInputError::UnmappedEvdevKey { evdev_code })?;
    let mut flags = KEYEVENTF_SCANCODE;
    if key.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: 0,
        wScan: key.scan_code,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: EDGE_KVM_INJECTION_TAG,
    };
    send_inputs(&[input], SendInput)
}

#[cfg(not(windows))]
fn send_key(evdev_code: u16, _down: bool) -> Result<()> {
    Err(WindowsInputError::UnmappedEvdevKey { evdev_code })
}

#[cfg(windows)]
fn send_inputs(
    inputs: &[windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT],
    send: unsafe extern "system" fn(
        u32,
        *const windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT,
        i32,
    ) -> u32,
) -> Result<()> {
    let sent = unsafe {
        send(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT>() as i32,
        )
    };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(WindowsInputError::Injection(format!(
            "SendInput accepted {sent} of {} events (UIPI may be blocking injection)",
            inputs.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_rounding_is_bounded() {
        assert_eq!(rounded_i32(1.6), 2);
        assert_eq!(rounded_i32(f64::MAX), i32::MAX);
        assert_eq!(rounded_i32(f64::MIN), i32::MIN);
    }

    #[test]
    fn every_mouse_button_has_a_stable_slot() {
        for (index, button) in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(button_index(button), index);
            assert_eq!(button_from_index(index), button);
        }
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_only_our_injected_input_tag() {
        assert!(is_edge_kvm_injected(EDGE_KVM_INJECTION_TAG));
        assert!(!is_edge_kvm_injected(0));
        assert!(!is_edge_kvm_injected(EDGE_KVM_INJECTION_TAG + 1));
    }
}
