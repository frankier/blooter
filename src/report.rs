//! Session input state and translation of Linux input events into HID Boot
//! input reports. See design/ARCH.md §5 and §7.

use crate::config::{Action, Hotkeys};
use crate::keymap;

// Linux event types (from <linux/input-event-codes.h>).
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

/// A decoded Linux `struct input_event`. Keyboard/mouse/touchpad events carry
/// `gamepad: None` and merge into one logical device; gamepad events carry
/// `gamepad: Some(slot)` identifying which controller they came from (their
/// `EV_ABS` stick/trigger values are pre-normalized to 0..=255 by the reader).
#[derive(Clone, Copy, Debug)]
pub struct RawEvent {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
    pub gamepad: Option<u8>,
}

/// One gamepad's accumulated control state. A gamepad input report is a full
/// snapshot (buttons + hat + sticks + triggers), re-sent whenever any control
/// changes.
#[derive(Clone, Copy)]
pub struct GamepadState {
    buttons: u16,
    /// Directional-pad axes, each in {-1, 0, 1}; combined into a HID hat value.
    hat_x: i8,
    hat_y: i8,
    /// Sticks (lx/ly = left, rx/ry = right) and triggers (lt/rt), 0..=255 with
    /// stick centre at 128.
    lx: u8,
    ly: u8,
    rx: u8,
    ry: u8,
    lt: u8,
    rt: u8,
}

impl GamepadState {
    fn neutral() -> Self {
        Self {
            buttons: 0,
            hat_x: 0,
            hat_y: 0,
            lx: 128,
            ly: 128,
            rx: 128,
            ry: 128,
            lt: 0,
            rt: 0,
        }
    }

    /// The 8-direction HID hat value (0..=7), or 8 for centred.
    fn hat(&self) -> u8 {
        match (self.hat_x, self.hat_y) {
            (0, -1) => 0,
            (1, -1) => 1,
            (1, 0) => 2,
            (1, 1) => 3,
            (0, 1) => 4,
            (-1, 1) => 5,
            (-1, 0) => 6,
            (-1, -1) => 7,
            _ => 8,
        }
    }

    /// Build this gamepad's input report (11 bytes incl. the `0xA1` HIDP header)
    /// for the given slot. Layout matches `sdp::gamepad_block`.
    fn report(&self, slot: u8) -> [u8; 11] {
        let [b_lo, b_hi] = self.buttons.to_le_bytes();
        [
            0xA1,
            crate::sdp::GAMEPAD_REPORT_ID_BASE + slot,
            b_lo,
            b_hi,
            self.hat(),
            self.lx,
            self.ly,
            self.rx,
            self.ry,
            self.lt,
            self.rt,
        ]
    }
}

/// The per-session state, reset on every new host connection (design/ARCH.md §7).
pub struct InputState {
    pub mouse_buttons: u8,
    pub modifiers: u8,
    pub pressed_keys: [u8; 8],
    /// Whether input is currently forwarded to the host. Key/button state is
    /// still tracked while off, so hotkey chords keep working.
    pub capture: bool,
    /// Touchpad state: finger down (BTN_TOUCH), and the last absolute
    /// position per axis, from which relative motion is derived.
    touching: bool,
    last_abs: [Option<i32>; 2],
    /// One entry per advertised gamepad slot (empty when gamepad forwarding is
    /// off), indexed by the `slot` carried on gamepad `RawEvent`s.
    gamepads: Box<[GamepadState]>,
}

impl Default for InputState {
    fn default() -> Self {
        Self::with_gamepads(0)
    }
}

impl InputState {
    /// Session state advertising `n_gamepads` gamepad slots.
    pub fn with_gamepads(n_gamepads: usize) -> Self {
        Self {
            mouse_buttons: 0,
            modifiers: 0,
            pressed_keys: [0; 8],
            capture: true,
            touching: false,
            last_abs: [None; 2],
            gamepads: vec![GamepadState::neutral(); n_gamepads].into_boxed_slice(),
        }
    }
}

/// Result of translating one event.
pub enum Outcome {
    /// Nothing to send.
    Nothing,
    /// A mouse input report (6 bytes incl. the `0xA1` HIDP header).
    Mouse([u8; 6]),
    /// A keyboard input report (11 bytes incl. the `0xA1` HIDP header).
    Keyboard([u8; 11]),
    /// A gamepad input report (11 bytes incl. the `0xA1` HIDP header).
    Gamepad([u8; 11]),
    /// The drop-connection hotkey fired: drop the current session.
    DropSession,
    /// The exit hotkey fired: terminate the whole program.
    Exit,
    /// Input capture was re-enabled (re-grab devices under -x).
    CaptureOn,
    /// Input capture was disabled (release -x grabs); send an all-keys-up
    /// report if connected.
    CaptureOff,
}

impl InputState {
    pub fn reset(&mut self) {
        self.mouse_buttons = 0;
        self.modifiers = 0;
        self.pressed_keys = [0; 8];
        self.capture = true;
        self.touching = false;
        self.last_abs = [None; 2];
        for gp in self.gamepads.iter_mut() {
            *gp = GamepadState::neutral();
        }
    }

    /// A neutral input report for every advertised gamepad, to release any held
    /// controller state host-side (on drop/exit/capture-off).
    pub fn gamepad_neutral_reports(&self) -> Vec<[u8; 11]> {
        let neutral = GamepadState::neutral();
        (0..self.gamepads.len())
            .map(|slot| neutral.report(slot as u8))
            .collect()
    }

    /// Build a mouse report from the current button state and given axes.
    pub fn mouse_report(&self, x: i8, y: i8, wheel: i8) -> [u8; 6] {
        [
            0xA1,
            0x01,
            self.mouse_buttons,
            x as u8,
            y as u8,
            wheel as u8,
        ]
    }

    /// Build a keyboard report from the current modifier and key state.
    pub fn keyboard_report(&self) -> [u8; 11] {
        let mut r = [0u8; 11];
        r[0] = 0xA1;
        r[1] = 0x02;
        r[2] = self.modifiers;
        r[3..11].copy_from_slice(&self.pressed_keys);
        r
    }

    /// An all-keys-up keyboard report (modifiers 0, no keys).
    pub fn keys_up_report() -> [u8; 11] {
        let mut r = [0u8; 11];
        r[0] = 0xA1;
        r[1] = 0x02;
        r
    }

    fn press(&mut self, usage: u8) {
        if self.pressed_keys.contains(&usage) {
            return;
        }
        if let Some(slot) = self.pressed_keys.iter_mut().find(|k| **k == 0) {
            *slot = usage;
        }
        // All 8 slots full: silently drop (no rollover-error reporting).
    }

    fn release(&mut self, usage: u8) {
        if let Some(idx) = self.pressed_keys.iter().position(|k| *k == usage) {
            for i in idx..7 {
                self.pressed_keys[i] = self.pressed_keys[i + 1];
            }
            self.pressed_keys[7] = 0;
        }
    }
}

/// Translate one raw input event, updating `state`. Returns the report (if any)
/// to transmit, or a hotkey action.
pub fn translate(hotkeys: &Hotkeys, state: &mut InputState, ev: RawEvent) -> Outcome {
    if let Some(slot) = ev.gamepad {
        return translate_gamepad(state, slot, ev.type_, ev.code, ev.value);
    }
    match ev.type_ {
        EV_KEY => translate_key(hotkeys, state, ev.code, ev.value),
        EV_REL => translate_rel(state, ev.code, ev.value),
        EV_ABS => translate_abs(state, ev.code, ev.value),
        _ => Outcome::Nothing,
    }
}

/// Update one gamepad's state from a button or (pre-normalized) axis event and,
/// while capturing, emit its full input report. Stick/trigger `value`s arrive
/// already scaled to 0..=255; hat axes arrive as -1/0/1.
fn translate_gamepad(
    state: &mut InputState,
    slot: u8,
    type_: u16,
    code: u16,
    value: i32,
) -> Outcome {
    let capture = state.capture;
    let Some(gp) = state.gamepads.get_mut(slot as usize) else {
        return Outcome::Nothing;
    };
    let recognized = match type_ {
        EV_KEY => match keymap::gamepad_button_bit(code) {
            Some(bit) => {
                if value != 0 {
                    gp.buttons |= 1 << bit;
                } else {
                    gp.buttons &= !(1 << bit);
                }
                true
            }
            None => false,
        },
        EV_ABS => {
            let v = value.clamp(0, 255) as u8;
            match code {
                keymap::ABS_X => gp.lx = v,
                keymap::ABS_Y => gp.ly = v,
                keymap::ABS_RX => gp.rx = v,
                keymap::ABS_RY => gp.ry = v,
                keymap::ABS_Z => gp.lt = v,
                keymap::ABS_RZ => gp.rt = v,
                keymap::ABS_HAT0X => gp.hat_x = value.signum() as i8,
                keymap::ABS_HAT0Y => gp.hat_y = value.signum() as i8,
                _ => return Outcome::Nothing,
            }
            true
        }
        _ => false,
    };
    if recognized && capture {
        Outcome::Gamepad(gp.report(slot))
    } else {
        Outcome::Nothing
    }
}

fn translate_key(hotkeys: &Hotkeys, state: &mut InputState, code: u16, value: i32) -> Outcome {
    use keymap::*;
    match code {
        BTN_LEFT | BTN_RIGHT | BTN_MIDDLE => {
            let bit = match code {
                BTN_LEFT => 0,
                BTN_RIGHT => 1,
                _ => 2,
            };
            if value != 0 {
                state.mouse_buttons |= 1 << bit;
            } else {
                state.mouse_buttons &= !(1 << bit);
            }
            if state.capture {
                Outcome::Mouse(state.mouse_report(0, 0, 0))
            } else {
                Outcome::Nothing
            }
        }
        BTN_TOUCH => {
            state.touching = value != 0;
            state.last_abs = [None; 2];
            Outcome::Nothing
        }
        // Hotkey trigger keys are consumed locally, never forwarded, and act
        // only on release (value 0).
        _ if hotkeys.is_trigger(code) => {
            if value != 0 {
                return Outcome::Nothing;
            }
            match hotkeys.action(code, state.modifiers) {
                Some(Action::Exit) => Outcome::Exit,
                Some(Action::DropConnection) => Outcome::DropSession,
                Some(Action::CaptureOn) => {
                    state.capture = true;
                    Outcome::CaptureOn
                }
                Some(Action::CaptureOff) => {
                    state.capture = false;
                    Outcome::CaptureOff
                }
                Some(Action::CaptureToggle) => {
                    state.capture = !state.capture;
                    if state.capture {
                        Outcome::CaptureOn
                    } else {
                        Outcome::CaptureOff
                    }
                }
                None => Outcome::Nothing,
            }
        }
        _ => {
            if let Some(bit) = modifier_bit(code) {
                if value >= 1 {
                    state.modifiers |= 1 << bit;
                } else {
                    state.modifiers &= !(1 << bit);
                }
            } else if let Some(usage) = hid_usage(code) {
                match value {
                    1 => state.press(usage),
                    0 => state.release(usage),
                    _ => {} // autorepeat (value 2): no list change, still report
                }
            } else {
                return Outcome::Nothing;
            }
            if state.capture {
                Outcome::Keyboard(state.keyboard_report())
            } else {
                Outcome::Nothing
            }
        }
    }
}

fn translate_rel(state: &mut InputState, code: u16, value: i32) -> Outcome {
    if !state.capture {
        return Outcome::Nothing;
    }
    let v = value.clamp(-127, 127) as i8;
    match code {
        keymap::REL_X => Outcome::Mouse(state.mouse_report(v, 0, 0)),
        keymap::REL_Y => Outcome::Mouse(state.mouse_report(0, v, 0)),
        keymap::REL_WHEEL => Outcome::Mouse(state.mouse_report(0, 0, v)),
        _ => Outcome::Nothing,
    }
}

/// Touchpads report absolute finger positions (single-touch emulation axes);
/// derive relative motion from consecutive positions while a finger is down.
/// The first position after touch-down only seeds the reference, so a landing
/// finger does not jump the pointer.
fn translate_abs(state: &mut InputState, code: u16, value: i32) -> Outcome {
    let axis = match code {
        keymap::ABS_X => 0,
        keymap::ABS_Y => 1,
        _ => return Outcome::Nothing,
    };
    if !state.touching {
        return Outcome::Nothing;
    }
    let prev = state.last_abs[axis].replace(value);
    let Some(prev) = prev else {
        return Outcome::Nothing;
    };
    if !state.capture {
        return Outcome::Nothing;
    }
    let d = (value - prev).clamp(-127, 127) as i8;
    if axis == 0 {
        Outcome::Mouse(state.mouse_report(d, 0, 0))
    } else {
        Outcome::Mouse(state.mouse_report(0, d, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            type_: EV_KEY,
            code,
            value,
            gamepad: None,
        }
    }

    #[test]
    fn press_release_key() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Press 'a' → usage 4 in slot 0.
        match translate(&hk, &mut s, key(keymap::KEY_A, 1)) {
            Outcome::Keyboard(r) => assert_eq!(&r[..5], &[0xA1, 0x02, 0x00, 4, 0]),
            _ => panic!(),
        }
        // Press 'b' → usage 5 appended.
        translate(&hk, &mut s, key(keymap::KEY_B, 1));
        assert_eq!(&s.pressed_keys[..2], &[4, 5]);
        // Release 'a' → shift left.
        translate(&hk, &mut s, key(keymap::KEY_A, 0));
        assert_eq!(&s.pressed_keys[..2], &[5, 0]);
    }

    #[test]
    fn autorepeat_no_change() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        translate(&hk, &mut s, key(keymap::KEY_A, 1));
        translate(&hk, &mut s, key(keymap::KEY_A, 2));
        assert_eq!(&s.pressed_keys[..2], &[4, 0]);
    }

    #[test]
    fn modifiers_and_hotkeys() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        translate(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 1));
        translate(&hk, &mut s, key(keymap::KEY_LEFTALT, 1));
        assert_eq!(s.modifiers, 0x05);
        // Ctrl+Alt held → Right Shift release exits.
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)),
            Outcome::Exit
        ));
        // Without both modifiers → nothing (drop_connection disabled by default).
        s.reset();
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)),
            Outcome::Nothing
        ));
    }

    #[test]
    fn capture_toggle() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Shift+Right Shift → capture off.
        translate(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1));
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)),
            Outcome::CaptureOff
        ));
        translate(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 0));
        assert!(!s.capture);
        // While off: keys and motion are tracked but not forwarded.
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_A, 1)),
            Outcome::Nothing
        ));
        assert_eq!(s.pressed_keys[0], 4);
        let rel = RawEvent {
            type_: EV_REL,
            code: keymap::REL_X,
            value: 5,
            gamepad: None,
        };
        assert!(matches!(translate(&hk, &mut s, rel), Outcome::Nothing));
        // Exit hotkey still works while off.
        translate(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 1));
        translate(&hk, &mut s, key(keymap::KEY_LEFTALT, 1));
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)),
            Outcome::Exit
        ));
        translate(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 0));
        translate(&hk, &mut s, key(keymap::KEY_LEFTALT, 0));
        // Shift+Right Shift again → capture back on.
        translate(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1));
        assert!(matches!(
            translate(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)),
            Outcome::CaptureOn
        ));
        assert!(s.capture);
    }

    #[test]
    fn touchpad_abs_to_rel() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        let abs = |code, value| RawEvent {
            type_: EV_ABS,
            code,
            value,
            gamepad: None,
        };
        // Motion without a finger down is ignored.
        assert!(matches!(
            translate(&hk, &mut s, abs(keymap::ABS_X, 500)),
            Outcome::Nothing
        ));
        // Finger down: first position only seeds the reference.
        translate(&hk, &mut s, key(keymap::BTN_TOUCH, 1));
        assert!(matches!(
            translate(&hk, &mut s, abs(keymap::ABS_X, 1000)),
            Outcome::Nothing
        ));
        assert!(matches!(
            translate(&hk, &mut s, abs(keymap::ABS_Y, 800)),
            Outcome::Nothing
        ));
        // Subsequent positions produce relative motion.
        match translate(&hk, &mut s, abs(keymap::ABS_X, 1010)) {
            Outcome::Mouse(r) => assert_eq!(r, [0xA1, 0x01, 0x00, 10, 0, 0]),
            _ => panic!(),
        }
        match translate(&hk, &mut s, abs(keymap::ABS_Y, 795)) {
            Outcome::Mouse(r) => assert_eq!(r, [0xA1, 0x01, 0x00, 0, -5i8 as u8, 0]),
            _ => panic!(),
        }
        // Lifting resets the reference: no jump on the next touch.
        translate(&hk, &mut s, key(keymap::BTN_TOUCH, 0));
        translate(&hk, &mut s, key(keymap::BTN_TOUCH, 1));
        assert!(matches!(
            translate(&hk, &mut s, abs(keymap::ABS_X, 2000)),
            Outcome::Nothing
        ));
    }

    #[test]
    fn mouse_rel_clamped() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        let ev = RawEvent {
            type_: EV_REL,
            code: keymap::REL_X,
            value: 5000,
            gamepad: None,
        };
        match translate(&hk, &mut s, ev) {
            Outcome::Mouse(r) => assert_eq!(r, [0xA1, 0x01, 0x00, 127, 0, 0]),
            _ => panic!(),
        }
    }

    #[test]
    fn keyboard_full_slots() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        for code in [
            keymap::KEY_A,
            keymap::KEY_B,
            keymap::KEY_C,
            keymap::KEY_D,
            keymap::KEY_E,
            keymap::KEY_F,
            keymap::KEY_G,
            keymap::KEY_H,
            keymap::KEY_I, // 9th, should be dropped
        ] {
            translate(&hk, &mut s, key(code, 1));
        }
        assert_eq!(s.pressed_keys, [4, 5, 6, 7, 8, 9, 10, 11]);
    }

    fn gp(slot: u8, type_: u16, code: u16, value: i32) -> RawEvent {
        RawEvent {
            type_,
            code,
            value,
            gamepad: Some(slot),
        }
    }

    #[test]
    fn gamepad_button_and_axes() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(2);
        // Press BTN_SOUTH (bit 0) on slot 0 → report ID 3, buttons_lo bit 0.
        match translate(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 1)) {
            // [header, id, b_lo, b_hi, hat, lx, ly, rx, ry, lt, rt]
            Outcome::Gamepad(r) => {
                assert_eq!(r[..5], [0xA1, 0x03, 0x01, 0x00, 8]);
                // Sticks neutral (128), triggers 0.
                assert_eq!(r[5..], [128, 128, 128, 128, 0, 0]);
            }
            _ => panic!(),
        }
        // Left stick X (already normalized) on slot 0.
        match translate(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_X, 200)) {
            Outcome::Gamepad(r) => assert_eq!(r[5], 200),
            _ => panic!(),
        }
        // Release the button.
        match translate(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 0)) {
            Outcome::Gamepad(r) => assert_eq!(r[2], 0x00),
            _ => panic!(),
        }
        // Slot 1 is independent and uses report ID 4.
        match translate(&hk, &mut s, gp(1, EV_KEY, keymap::BTN_START, 1)) {
            Outcome::Gamepad(r) => assert_eq!(r[..4], [0xA1, 0x04, 0x00, 0x08]),
            _ => panic!(),
        }
    }

    #[test]
    fn gamepad_hat_encoding() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(1);
        // Up-left → NW (7).
        translate(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0X, -1));
        match translate(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0Y, -1)) {
            Outcome::Gamepad(r) => assert_eq!(r[4], 7),
            _ => panic!(),
        }
        // Back to centre → 8.
        translate(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0X, 0));
        match translate(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0Y, 0)) {
            Outcome::Gamepad(r) => assert_eq!(r[4], 8),
            _ => panic!(),
        }
    }

    #[test]
    fn gamepad_out_of_range_slot_ignored() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(1);
        assert!(matches!(
            translate(&hk, &mut s, gp(5, EV_KEY, keymap::BTN_SOUTH, 1)),
            Outcome::Nothing
        ));
    }

    #[test]
    fn gamepad_not_forwarded_while_capture_off() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(1);
        s.capture = false;
        // State still tracked, but nothing forwarded.
        assert!(matches!(
            translate(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 1)),
            Outcome::Nothing
        ));
        s.capture = true;
        // The tracked press is reflected once capturing resumes.
        match translate(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_EAST, 1)) {
            Outcome::Gamepad(r) => assert_eq!(r[2], 0b11), // south + east
            _ => panic!(),
        }
    }
}
