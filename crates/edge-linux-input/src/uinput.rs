use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use edge_protocol::{InputEvent as ProtocolInputEvent, MouseButton};
use evdev::{
    AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};

use crate::{LinuxInputError, Result};

const EVDEV_KEY_MAX: u16 = 0x2ff;

#[derive(Debug, Clone)]
pub struct UinputBackend {
    sender: Arc<Mutex<UinputSender>>,
}

impl UinputBackend {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            sender: Arc::new(Mutex::new(UinputSender::connect()?)),
        })
    }

    pub async fn inject(&self, event: ProtocolInputEvent) -> Result<()> {
        self.sender
            .lock()
            .map_err(|_| LinuxInputError::UinputLockPoisoned)?
            .inject(event)
    }

    pub async fn all_keys_up(&self) -> Result<()> {
        self.inject(ProtocolInputEvent::AllKeysUp).await
    }
}

#[derive(Debug)]
struct UinputSender {
    device: VirtualDevice,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    motion_remainder: (f64, f64),
    wheel_remainder: (f64, f64),
}

impl UinputSender {
    fn connect() -> Result<Self> {
        let keys = AttributeSet::from_iter((1..=EVDEV_KEY_MAX).map(KeyCode));
        let axes = AttributeSet::from_iter([
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
        ]);
        let device = VirtualDevice::builder()
            .and_then(|builder| builder.name("edge-kvm receiver").with_keys(&keys))
            .and_then(|builder| builder.with_relative_axes(&axes))
            .and_then(|builder| builder.build())
            .map_err(|error| LinuxInputError::UinputInit(error.to_string()))?;

        tracing::info!(device = "/dev/uinput", "using Linux uinput backend");
        Ok(Self {
            device,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            motion_remainder: (0.0, 0.0),
            wheel_remainder: (0.0, 0.0),
        })
    }

    fn inject(&mut self, event: ProtocolInputEvent) -> Result<()> {
        match event {
            ProtocolInputEvent::PointerMotion { dx, dy } => {
                let (x, y) = integral_delta(&mut self.motion_remainder, dx, dy);
                self.emit_relative(x, y, RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y)
            }
            ProtocolInputEvent::PointerWheel { x, y } => {
                let (x, y) = integral_delta(&mut self.wheel_remainder, x, y);
                self.emit_relative(
                    x,
                    y,
                    RelativeAxisCode::REL_HWHEEL,
                    RelativeAxisCode::REL_WHEEL,
                )
            }
            ProtocolInputEvent::PointerButton { button, down } => {
                let code = mouse_button_code(button);
                self.emit_key(code, down)?;
                if down {
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                }
                Ok(())
            }
            ProtocolInputEvent::Key { evdev_code, down } => {
                if evdev_code > EVDEV_KEY_MAX {
                    return Err(LinuxInputError::UinputUnsupportedKey(evdev_code));
                }
                self.emit_key(evdev_code, down)?;
                if down {
                    self.pressed_keys.insert(evdev_code);
                } else {
                    self.pressed_keys.remove(&evdev_code);
                }
                Ok(())
            }
            ProtocolInputEvent::AllKeysUp => self.release_all(),
        }
    }

    fn emit_relative(
        &mut self,
        x: i32,
        y: i32,
        x_axis: RelativeAxisCode,
        y_axis: RelativeAxisCode,
    ) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if x != 0 {
            events.push(InputEvent::new(EventType::RELATIVE.0, x_axis.0, x));
        }
        if y != 0 {
            events.push(InputEvent::new(EventType::RELATIVE.0, y_axis.0, y));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.device.emit(&events).map_err(LinuxInputError::Io)
    }

    fn emit_key(&mut self, code: u16, down: bool) -> Result<()> {
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code, i32::from(down))])
            .map_err(LinuxInputError::Io)
    }

    fn release_all(&mut self) -> Result<()> {
        let events: Vec<_> = self
            .pressed_keys
            .iter()
            .chain(&self.pressed_buttons)
            .map(|code| InputEvent::new(EventType::KEY.0, *code, 0))
            .collect();
        self.motion_remainder = (0.0, 0.0);
        self.wheel_remainder = (0.0, 0.0);
        if events.is_empty() {
            return Ok(());
        }
        self.device.emit(&events).map_err(LinuxInputError::Io)?;
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        Ok(())
    }
}

impl Drop for UinputSender {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn integral_delta(remainder: &mut (f64, f64), x: f64, y: f64) -> (i32, i32) {
    remainder.0 += x;
    remainder.1 += y;
    let integral = (remainder.0.trunc() as i32, remainder.1.trunc() as i32);
    remainder.0 -= f64::from(integral.0);
    remainder.1 -= f64::from(integral.1);
    integral
}

fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => KeyCode::BTN_LEFT.0,
        MouseButton::Right => KeyCode::BTN_RIGHT.0,
        MouseButton::Middle => KeyCode::BTN_MIDDLE.0,
        MouseButton::Back => KeyCode::BTN_BACK.0,
        MouseButton::Forward => KeyCode::BTN_FORWARD.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_deltas_are_preserved_until_they_form_an_event() {
        let mut remainder = (0.0, 0.0);
        assert_eq!(integral_delta(&mut remainder, 0.4, -0.4), (0, 0));
        assert_eq!(integral_delta(&mut remainder, 0.7, -0.7), (1, -1));
        assert!((remainder.0 - 0.1).abs() < f64::EPSILON * 2.0);
        assert!((remainder.1 + 0.1).abs() < f64::EPSILON * 2.0);
    }

    #[test]
    fn mouse_buttons_use_linux_input_codes() {
        assert_eq!(mouse_button_code(MouseButton::Left), 0x110);
        assert_eq!(mouse_button_code(MouseButton::Right), 0x111);
        assert_eq!(mouse_button_code(MouseButton::Middle), 0x112);
        assert_eq!(mouse_button_code(MouseButton::Forward), 0x115);
        assert_eq!(mouse_button_code(MouseButton::Back), 0x116);
    }
}
