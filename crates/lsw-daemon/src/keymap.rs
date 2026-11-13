//! Linux evdev key codes → QEMU "qnum" key codes (PC scan code set 1, with
//! 0xe0-prefixed keys folded into 0x80 | code), as `org.qemu.Display1.Keyboard`
//! expects. GDK reports evdev codes plus 8 as hardware keycodes on Wayland
//! and X11.

/// qnum for an evdev key code, or `None` for keys a PC keyboard lacks.
pub fn evdev_to_qnum(code: u32) -> Option<u32> {
    // Keys 1..=83 (Esc .. keypad '.') are identical in evdev and set 1.
    if (1..=83).contains(&code) {
        return Some(code);
    }
    Some(match code {
        86 => 0x56,  // KEY_102ND
        87 => 0x57,  // F11
        88 => 0x58,  // F12
        89 => 0x73,  // KEY_RO
        92 => 0x79,  // KEY_HENKAN
        93 => 0x70,  // KEY_KATAKANAHIRAGANA
        94 => 0x7b,  // KEY_MUHENKAN
        96 => 0x9c,  // KP Enter
        97 => 0x9d,  // Right Ctrl
        98 => 0xb5,  // KP /
        99 => 0xb7,  // SysRq / Print Screen
        100 => 0xb8, // Right Alt
        102 => 0xc7, // Home
        103 => 0xc8, // Up
        104 => 0xc9, // Page Up
        105 => 0xcb, // Left
        106 => 0xcd, // Right
        107 => 0xcf, // End
        108 => 0xd0, // Down
        109 => 0xd1, // Page Down
        110 => 0xd2, // Insert
        111 => 0xd3, // Delete
        113 => 0xa0, // Mute
        114 => 0xae, // Volume Down
        115 => 0xb0, // Volume Up
        117 => 0x59, // KP =
        119 => 0xc6, // Pause
        124 => 0x7d, // KEY_YEN
        125 => 0xdb, // Left Super (Windows key)
        126 => 0xdc, // Right Super
        127 => 0xdd, // Menu
        _ => return None,
    })
}

/// QEMU InputButton values used by `org.qemu.Display1.Mouse`.
pub mod button {
    pub const LEFT: u32 = 0;
    pub const MIDDLE: u32 = 1;
    pub const RIGHT: u32 = 2;
    pub const WHEEL_UP: u32 = 3;
    pub const WHEEL_DOWN: u32 = 4;
    pub const SIDE: u32 = 5;
    pub const EXTRA: u32 = 6;
    pub const WHEEL_LEFT: u32 = 7;
    pub const WHEEL_RIGHT: u32 = 8;
}

/// GDK mouse button number (1 = primary, 2 = middle, 3 = secondary, 8/9 =
/// back/forward) → QEMU button.
pub fn gdk_button_to_qemu(b: u32) -> Option<u32> {
    match b {
        1 => Some(button::LEFT),
        2 => Some(button::MIDDLE),
        3 => Some(button::RIGHT),
        8 => Some(button::SIDE),
        9 => Some(button::EXTRA),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_keys_are_identity_and_extended_keys_are_e0() {
        assert_eq!(evdev_to_qnum(1), Some(1)); // Esc
        assert_eq!(evdev_to_qnum(30), Some(30)); // A
        assert_eq!(evdev_to_qnum(103), Some(0xc8)); // Up = e0 48
        assert_eq!(evdev_to_qnum(125), Some(0xdb)); // Super = e0 5b
        assert_eq!(evdev_to_qnum(97), Some(0x9d)); // Right Ctrl = e0 1d
        assert_eq!(evdev_to_qnum(0), None);
        assert_eq!(evdev_to_qnum(240), None);
    }

    #[test]
    fn mapped_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in 0..256 {
            if let Some(q) = evdev_to_qnum(code) {
                assert!(seen.insert(q), "qnum {q:#x} mapped twice");
            }
        }
    }

    #[test]
    fn buttons() {
        assert_eq!(gdk_button_to_qemu(1), Some(button::LEFT));
        assert_eq!(gdk_button_to_qemu(3), Some(button::RIGHT));
        assert_eq!(gdk_button_to_qemu(4), None);
    }
}
