use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowsScanCode {
    pub scan_code: u16,
    pub extended: bool,
}

impl WindowsScanCode {
    pub const fn new(scan_code: u16, extended: bool) -> Self {
        Self {
            scan_code,
            extended,
        }
    }
}

pub fn windows_scancode_to_evdev(key: WindowsScanCode) -> Option<u16> {
    let code = match (key.scan_code, key.extended) {
        // Number row.
        (0x02, false) => 2,
        (0x03, false) => 3,
        (0x04, false) => 4,
        (0x05, false) => 5,
        (0x06, false) => 6,
        (0x07, false) => 7,
        (0x08, false) => 8,
        (0x09, false) => 9,
        (0x0a, false) => 10,
        (0x0b, false) => 11,
        // Letters.
        (0x10, false) => 16,
        (0x11, false) => 17,
        (0x12, false) => 18,
        (0x13, false) => 19,
        (0x14, false) => 20,
        (0x15, false) => 21,
        (0x16, false) => 22,
        (0x17, false) => 23,
        (0x18, false) => 24,
        (0x19, false) => 25,
        (0x1e, false) => 30,
        (0x1f, false) => 31,
        (0x20, false) => 32,
        (0x21, false) => 33,
        (0x22, false) => 34,
        (0x23, false) => 35,
        (0x24, false) => 36,
        (0x25, false) => 37,
        (0x26, false) => 38,
        (0x2c, false) => 44,
        (0x2d, false) => 45,
        (0x2e, false) => 46,
        (0x2f, false) => 47,
        (0x30, false) => 48,
        (0x31, false) => 49,
        (0x32, false) => 50,
        // Editing and whitespace.
        (0x01, false) => 1,
        (0x0e, false) => 14,
        (0x0f, false) => 15,
        (0x1c, false) => 28,
        (0x39, false) => 57,
        (0x1c, true) => 28,
        // Modifiers.
        (0x2a, false) => 42,
        (0x36, false) => 54,
        (0x1d, false) => 29,
        (0x1d, true) => 97,
        (0x38, false) => 56,
        (0x38, true) => 100,
        (0x5b, true) => 125,
        (0x5c, true) => 126,
        // Arrows and navigation.
        (0x48, true) => 103,
        (0x50, true) => 108,
        (0x4b, true) => 105,
        (0x4d, true) => 106,
        (0x52, true) => 110,
        (0x53, true) => 111,
        (0x47, true) => 102,
        (0x4f, true) => 107,
        (0x49, true) => 104,
        (0x51, true) => 109,
        // F1-F12.
        (0x3b, false) => 59,
        (0x3c, false) => 60,
        (0x3d, false) => 61,
        (0x3e, false) => 62,
        (0x3f, false) => 63,
        (0x40, false) => 64,
        (0x41, false) => 65,
        (0x42, false) => 66,
        (0x43, false) => 67,
        (0x44, false) => 68,
        (0x57, false) => 87,
        (0x58, false) => 88,
        // Common punctuation.
        (0x0c, false) => 12,
        (0x0d, false) => 13,
        (0x1a, false) => 26,
        (0x1b, false) => 27,
        (0x27, false) => 39,
        (0x28, false) => 40,
        (0x29, false) => 41,
        (0x2b, false) => 43,
        (0x33, false) => 51,
        (0x34, false) => 52,
        (0x35, false) => 53,
        _ => return None,
    };

    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mvp_keys() {
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x1e, false)),
            Some(30)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x2e, false)),
            Some(46)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x32, false)),
            Some(50)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x2b, false)),
            Some(43)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x1d, false)),
            Some(29)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x0e, false)),
            Some(14)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x1c, false)),
            Some(28)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x48, true)),
            Some(103)
        );
        assert_eq!(
            windows_scancode_to_evdev(WindowsScanCode::new(0x3b, false)),
            Some(59)
        );
    }
}
