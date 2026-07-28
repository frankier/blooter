//! Mapping from Linux input keycodes to USB HID usage codes (Keyboard/Keypad
//! page), and the modifier-bit assignment. See design/ARCH.md §7.2 / §7.4.

// Linux keycodes (from <linux/input-event-codes.h>) that we care about.
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_KPASTERISK: u16 = 55;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_KP7: u16 = 71;
pub const KEY_KP8: u16 = 72;
pub const KEY_KP9: u16 = 73;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KP4: u16 = 75;
pub const KEY_KP5: u16 = 76;
pub const KEY_KP6: u16 = 77;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_KP1: u16 = 79;
pub const KEY_KP2: u16 = 80;
pub const KEY_KP3: u16 = 81;
pub const KEY_KP0: u16 = 82;
pub const KEY_KPDOT: u16 = 83;
pub const KEY_102ND: u16 = 86;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_KPSLASH: u16 = 98;
pub const KEY_SYSRQ: u16 = 99;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

// Mouse buttons.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

// Touch contact (touchpads).
pub const BTN_TOUCH: u16 = 0x14a;

// Relative axes.
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;

// Absolute axes (touchpad single-touch emulation, and gamepad sticks/triggers).
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

// Joystick buttons (BTN_JOYSTICK range). Cheap/generic pads, flight sticks and
// arcade sticks report their buttons here instead of the BTN_GAMEPAD range.
// `BTN_TRIGGER` == `BTN_JOYSTICK`, the first code of the range.
pub const BTN_TRIGGER: u16 = 0x120;
pub const BTN_JOYSTICK_LAST: u16 = 0x12f; // BTN_DEAD, last code before BTN_GAMEPAD

// Gamepad buttons (BTN_GAMEPAD range). A button anywhere in the joystick or
// gamepad range (0x120..=0x13f) marks a device as a gamepad during the input
// scan (see `input::is_gamepad`).
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_C: u16 = 0x132;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_GP_Z: u16 = 0x135;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;

/// Resolve a keyd-style key name to a Linux keycode. Both the primary and the
/// alternate spellings from keyd's keycode table are accepted (e.g. "esc" and
/// "escape", "-" and "minus"). Only keys blooter forwards resolve.
pub fn keycode_from_name(name: &str) -> Option<u16> {
    Some(match name {
        "esc" | "escape" => KEY_ESC,
        "1" => KEY_1,
        "2" => KEY_2,
        "3" => KEY_3,
        "4" => KEY_4,
        "5" => KEY_5,
        "6" => KEY_6,
        "7" => KEY_7,
        "8" => KEY_8,
        "9" => KEY_9,
        "0" => KEY_0,
        "-" | "minus" => KEY_MINUS,
        "=" | "equal" => KEY_EQUAL,
        "backspace" => KEY_BACKSPACE,
        "tab" => KEY_TAB,
        "q" => KEY_Q,
        "w" => KEY_W,
        "e" => KEY_E,
        "r" => KEY_R,
        "t" => KEY_T,
        "y" => KEY_Y,
        "u" => KEY_U,
        "i" => KEY_I,
        "o" => KEY_O,
        "p" => KEY_P,
        "[" | "leftbrace" => KEY_LEFTBRACE,
        "]" | "rightbrace" => KEY_RIGHTBRACE,
        "enter" => KEY_ENTER,
        "leftcontrol" => KEY_LEFTCTRL,
        "a" => KEY_A,
        "s" => KEY_S,
        "d" => KEY_D,
        "f" => KEY_F,
        "g" => KEY_G,
        "h" => KEY_H,
        "j" => KEY_J,
        "k" => KEY_K,
        "l" => KEY_L,
        ";" | "semicolon" => KEY_SEMICOLON,
        "'" | "apostrophe" => KEY_APOSTROPHE,
        "`" | "grave" => KEY_GRAVE,
        "leftshift" => KEY_LEFTSHIFT,
        "\\" | "backslash" => KEY_BACKSLASH,
        "z" => KEY_Z,
        "x" => KEY_X,
        "c" => KEY_C,
        "v" => KEY_V,
        "b" => KEY_B,
        "n" => KEY_N,
        "m" => KEY_M,
        "," | "comma" => KEY_COMMA,
        "." | "dot" => KEY_DOT,
        "/" | "slash" => KEY_SLASH,
        "rightshift" => KEY_RIGHTSHIFT,
        "kpasterisk" => KEY_KPASTERISK,
        "leftalt" => KEY_LEFTALT,
        "space" => KEY_SPACE,
        "capslock" => KEY_CAPSLOCK,
        "f1" => KEY_F1,
        "f2" => KEY_F2,
        "f3" => KEY_F3,
        "f4" => KEY_F4,
        "f5" => KEY_F5,
        "f6" => KEY_F6,
        "f7" => KEY_F7,
        "f8" => KEY_F8,
        "f9" => KEY_F9,
        "f10" => KEY_F10,
        "f11" => KEY_F11,
        "f12" => KEY_F12,
        "numlock" => KEY_NUMLOCK,
        "scrolllock" => KEY_SCROLLLOCK,
        "kp0" => KEY_KP0,
        "kp1" => KEY_KP1,
        "kp2" => KEY_KP2,
        "kp3" => KEY_KP3,
        "kp4" => KEY_KP4,
        "kp5" => KEY_KP5,
        "kp6" => KEY_KP6,
        "kp7" => KEY_KP7,
        "kp8" => KEY_KP8,
        "kp9" => KEY_KP9,
        "kpminus" => KEY_KPMINUS,
        "kpplus" => KEY_KPPLUS,
        "kpdot" => KEY_KPDOT,
        "kpenter" => KEY_KPENTER,
        "kpslash" => KEY_KPSLASH,
        "102nd" => KEY_102ND,
        "rightcontrol" => KEY_RIGHTCTRL,
        "sysrq" => KEY_SYSRQ,
        "rightalt" => KEY_RIGHTALT,
        "home" => KEY_HOME,
        "up" => KEY_UP,
        "pageup" => KEY_PAGEUP,
        "left" => KEY_LEFT,
        "right" => KEY_RIGHT,
        "end" => KEY_END,
        "down" => KEY_DOWN,
        "pagedown" => KEY_PAGEDOWN,
        "insert" => KEY_INSERT,
        "delete" => KEY_DELETE,
        "pause" => KEY_PAUSE,
        "leftmeta" => KEY_LEFTMETA,
        "rightmeta" => KEY_RIGHTMETA,
        _ => return None,
    })
}

/// If `code` is one of the eight modifier keys, return the bit position it
/// occupies in the HID modifier bitmap (0..=7), else `None`.
pub fn modifier_bit(code: u16) -> Option<u8> {
    Some(match code {
        KEY_LEFTCTRL => 0,
        KEY_LEFTSHIFT => 1,
        KEY_LEFTALT => 2,
        KEY_LEFTMETA => 3,
        KEY_RIGHTCTRL => 4,
        KEY_RIGHTSHIFT => 5,
        KEY_RIGHTALT => 6,
        KEY_RIGHTMETA => 7,
        _ => return None,
    })
}

/// Translate a Linux keycode to a HID Keyboard/Keypad usage code (4..=99), or
/// `None` if unmapped. Keys taking part in a chord being made are consumed
/// before this lookup and never forwarded (design/ARCH.md §7.3).
pub fn hid_usage(code: u16) -> Option<u8> {
    Some(match code {
        // Letters, alphabetical → 4..=29.
        KEY_A => 4,
        KEY_B => 5,
        KEY_C => 6,
        KEY_D => 7,
        KEY_E => 8,
        KEY_F => 9,
        KEY_G => 10,
        KEY_H => 11,
        KEY_I => 12,
        KEY_J => 13,
        KEY_K => 14,
        KEY_L => 15,
        KEY_M => 16,
        KEY_N => 17,
        KEY_O => 18,
        KEY_P => 19,
        KEY_Q => 20,
        KEY_R => 21,
        KEY_S => 22,
        KEY_T => 23,
        KEY_U => 24,
        KEY_V => 25,
        KEY_W => 26,
        KEY_X => 27,
        KEY_Y => 28,
        KEY_Z => 29,
        // Digits 1..9,0 → 30..=39.
        KEY_1 => 30,
        KEY_2 => 31,
        KEY_3 => 32,
        KEY_4 => 33,
        KEY_5 => 34,
        KEY_6 => 35,
        KEY_7 => 36,
        KEY_8 => 37,
        KEY_9 => 38,
        KEY_0 => 39,
        KEY_ENTER => 40,
        KEY_ESC => 41,
        KEY_BACKSPACE => 42,
        KEY_TAB => 43,
        KEY_SPACE => 44,
        KEY_MINUS => 45,
        KEY_EQUAL => 46,
        KEY_LEFTBRACE => 47,
        KEY_RIGHTBRACE => 48,
        KEY_BACKSLASH => 49,
        KEY_102ND => 50,
        KEY_SEMICOLON => 51,
        KEY_APOSTROPHE => 52,
        KEY_GRAVE => 53,
        KEY_COMMA => 54,
        KEY_DOT => 55,
        KEY_SLASH => 56,
        KEY_CAPSLOCK => 57,
        // Function keys F1..F12 → 58..=69.
        KEY_F1 => 58,
        KEY_F2 => 59,
        KEY_F3 => 60,
        KEY_F4 => 61,
        KEY_F5 => 62,
        KEY_F6 => 63,
        KEY_F7 => 64,
        KEY_F8 => 65,
        KEY_F9 => 66,
        KEY_F10 => 67,
        KEY_F11 => 68,
        KEY_F12 => 69,
        KEY_SYSRQ => 70, // PrintScreen
        KEY_SCROLLLOCK => 71,
        KEY_PAUSE => 72,
        KEY_INSERT => 73,
        KEY_HOME => 74,
        KEY_PAGEUP => 75,
        KEY_DELETE => 76,
        KEY_END => 77,
        KEY_PAGEDOWN => 78,
        KEY_RIGHT => 79,
        KEY_LEFT => 80,
        KEY_DOWN => 81,
        KEY_UP => 82,
        KEY_NUMLOCK => 83,
        KEY_KPSLASH => 84,
        KEY_KPASTERISK => 85,
        KEY_KPMINUS => 86,
        KEY_KPPLUS => 87,
        KEY_KPENTER => 88,
        KEY_KP1 => 89,
        KEY_KP2 => 90,
        KEY_KP3 => 91,
        KEY_KP4 => 92,
        KEY_KP5 => 93,
        KEY_KP6 => 94,
        KEY_KP7 => 95,
        KEY_KP8 => 96,
        KEY_KP9 => 97,
        KEY_KP0 => 98,
        KEY_KPDOT => 99,
        _ => return None,
    })
}

/// The HID gamepad button bit (0..=15) a Linux `BTN_*` code maps to, or `None`
/// if the code is not one of the forwarded gamepad/joystick buttons. Modern
/// pads report the `BTN_GAMEPAD` range (`BTN_SOUTH` …); cheap/generic pads,
/// flight sticks and arcade sticks report the `BTN_JOYSTICK` range
/// (`BTN_TRIGGER` …). Both are mapped, sequentially, onto the report's 16
/// button bits — a device uses one range or the other, so the two never
/// collide in practice.
pub fn gamepad_button_bit(code: u16) -> Option<u8> {
    Some(match code {
        BTN_SOUTH => 0,
        BTN_EAST => 1,
        BTN_C => 2,
        BTN_NORTH => 3,
        BTN_WEST => 4,
        BTN_GP_Z => 5,
        BTN_TL => 6,
        BTN_TR => 7,
        BTN_TL2 => 8,
        BTN_TR2 => 9,
        BTN_SELECT => 10,
        BTN_START => 11,
        BTN_MODE => 12,
        BTN_THUMBL => 13,
        BTN_THUMBR => 14,
        // Joystick range: BTN_TRIGGER (0x120) → bit 0 … BTN_DEAD (0x12f) → 15.
        BTN_TRIGGER..=BTN_JOYSTICK_LAST => (code - BTN_TRIGGER) as u8,
        _ => return None,
    })
}

/// The gamepad stick/trigger axes, normalized to 0..=255 for the HID report.
pub fn is_stick_or_trigger(code: u16) -> bool {
    matches!(code, ABS_X | ABS_Y | ABS_Z | ABS_RX | ABS_RY | ABS_RZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_range_maps_from_btn_south() {
        assert_eq!(gamepad_button_bit(BTN_SOUTH), Some(0));
        assert_eq!(gamepad_button_bit(BTN_THUMBR), Some(14));
    }

    #[test]
    fn joystick_range_maps_sequentially() {
        // Cheap "USB gamepad" pads report BTN_TRIGGER.. instead of BTN_SOUTH..
        assert_eq!(gamepad_button_bit(BTN_TRIGGER), Some(0));
        assert_eq!(gamepad_button_bit(0x121), Some(1)); // BTN_THUMB
        assert_eq!(gamepad_button_bit(0x129), Some(9)); // BTN_BASE4
        assert_eq!(gamepad_button_bit(BTN_JOYSTICK_LAST), Some(15)); // BTN_DEAD
    }

    #[test]
    fn non_button_codes_unmapped() {
        assert_eq!(gamepad_button_bit(0x11f), None); // just below the range
        assert_eq!(gamepad_button_bit(0x13f), None); // just above BTN_THUMBR
        assert_eq!(gamepad_button_bit(BTN_TOUCH), None);
    }
}
