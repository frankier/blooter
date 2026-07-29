//! Session input state and translation of Linux input events into HID Boot
//! input reports. See design/ARCH.md §5 and §7.

use crate::config::{Action, AxisBits, Chord, Hotkeys, MAX_CHORD_KEYS, Overflow};
use crate::keymap;

// Linux event types (from <linux/input-event-codes.h>).
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

/// `EV_SYN` code marking the end of an input frame.
pub const SYN_REPORT: u16 = 0;

/// The largest input report blooter emits (keyboard and gamepad are both 11
/// bytes including the `0xA1` HIDP header; a mouse report is 6 or 8).
pub const MAX_REPORT: usize = 11;

/// One built HID input report, `[0xA1, report_id, payload…]`. Fixed-size so it
/// can sit in the outgoing ring without allocating (design/ARCH.md §7.2c).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Report {
    len: u8,
    bytes: [u8; MAX_REPORT],
}

impl Report {
    /// Build from a slice of at most `MAX_REPORT` bytes.
    pub fn new(src: &[u8]) -> Self {
        let mut bytes = [0u8; MAX_REPORT];
        bytes[..src.len()].copy_from_slice(src);
        Report {
            len: src.len() as u8,
            bytes,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The HID report id (byte 1, after the `0xA1` HIDP header).
    pub fn id(&self) -> u8 {
        self.bytes[1]
    }
}

impl std::ops::Deref for Report {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

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

/// Pointer motion accumulated since the last flush, in device units. Held as
/// `i32` so merging many frames cannot overflow the axis range mid-way; the
/// clamp to what one report can carry happens in `take_mouse_frame`
/// (design/ARCH.md §7.2c).
#[derive(Clone, Copy, Default)]
struct MouseAccum {
    x: i32,
    y: i32,
    wheel: i32,
}

impl MouseAccum {
    fn is_empty(&self) -> bool {
        self.x == 0 && self.y == 0 && self.wheel == 0
    }
}

/// A fixed-capacity set of keycodes in press order: the chord keys currently
/// held back, or those a fired chord consumed. Chords are capped at
/// [`MAX_CHORD_KEYS`] keys, so this never overflows and never allocates.
#[derive(Clone, Copy, Default)]
struct KeyVec {
    keys: [u16; MAX_CHORD_KEYS],
    len: u8,
}

impl KeyVec {
    fn as_slice(&self) -> &[u16] {
        &self.keys[..self.len as usize]
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn contains(&self, code: u16) -> bool {
        self.as_slice().contains(&code)
    }

    fn push(&mut self, code: u16) {
        if (self.len as usize) < MAX_CHORD_KEYS {
            self.keys[self.len as usize] = code;
            self.len += 1;
        }
    }

    fn remove(&mut self, code: u16) {
        if let Some(i) = self.as_slice().iter().position(|k| *k == code) {
            self.keys.copy_within(i + 1..self.len as usize, i);
            self.len -= 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// A chord still consistent with what has been pressed: its index in
/// `Hotkeys::chords`, and a bit per step already satisfied.
#[derive(Clone, Copy, Default)]
struct Cand {
    chord: u8,
    matched: u8,
}

/// Keys held back because they may yet complete a chord, plus the chords they
/// can still complete (design/ARCH.md §7.3). Empty when no chord is in progress.
#[derive(Clone, Copy, Default)]
struct ChordBuf {
    keys: KeyVec,
    cands: [Cand; crate::config::MAX_CHORDS],
    n_cands: u8,
}

impl ChordBuf {
    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.n_cands = 0;
    }

    fn cands(&self) -> &[Cand] {
        &self.cands[..self.n_cands as usize]
    }

    fn push_cand(&mut self, chord: u8, matched: u8) {
        if (self.n_cands as usize) < self.cands.len() {
            self.cands[self.n_cands as usize] = Cand { chord, matched };
            self.n_cands += 1;
        }
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
    /// Keys held back while they might still complete a chord (§7.3).
    chord: ChordBuf,
    /// Keys a fired chord consumed. Their downs were never forwarded, so their
    /// autorepeats and release are swallowed too: the host must not see a
    /// key-up it has no matching key-down for.
    chord_held: KeyVec,
    /// Touchpad state: finger down (BTN_TOUCH), and the last absolute
    /// position per axis, from which relative motion is derived.
    touching: bool,
    last_abs: [Option<i32>; 2],
    /// One entry per advertised gamepad slot (empty when gamepad forwarding is
    /// off), indexed by the `slot` carried on gamepad `RawEvent`s.
    gamepads: Box<[GamepadState]>,
    /// Pointer motion pending since the last frame boundary, and how it is
    /// encoded once a flush is due (§7.2c).
    accum: MouseAccum,
    /// Set when the button state changed this frame, so a click with no motion
    /// still produces a report.
    mouse_dirty: bool,
    axis_bits: AxisBits,
    overflow: Overflow,
}

impl Default for InputState {
    fn default() -> Self {
        Self::with_gamepads(0)
    }
}

impl InputState {
    /// Session state advertising `n_gamepads` gamepad slots, with 8-bit axes.
    pub fn with_gamepads(n_gamepads: usize) -> Self {
        Self {
            mouse_buttons: 0,
            modifiers: 0,
            pressed_keys: [0; 8],
            capture: true,
            chord: ChordBuf::default(),
            chord_held: KeyVec::default(),
            touching: false,
            last_abs: [None; 2],
            gamepads: vec![GamepadState::neutral(); n_gamepads].into_boxed_slice(),
            accum: MouseAccum::default(),
            mouse_dirty: false,
            axis_bits: AxisBits::Eight,
            overflow: Overflow::Burst,
        }
    }

    /// Set the pointer encoding from `[pointer] axis_bits` / `overflow`.
    pub fn with_pointer(mut self, axis_bits: AxisBits, overflow: Overflow) -> Self {
        self.axis_bits = axis_bits;
        self.overflow = overflow;
        self
    }

    pub fn axis_bits(&self) -> AxisBits {
        self.axis_bits
    }

    pub fn overflow(&self) -> Overflow {
        self.overflow
    }

    /// Whether any pointer motion or button change is waiting to be flushed.
    pub fn mouse_pending(&self) -> bool {
        self.mouse_dirty || !self.accum.is_empty()
    }

    /// Discard pending pointer state, so it cannot surface after a capture
    /// pause, a dropped session or a reconnect (design/ARCH.md §6.3).
    pub fn clear_mouse(&mut self) {
        self.accum = MouseAccum::default();
        self.mouse_dirty = false;
    }

    /// Take up to one report's worth of accumulated motion, clamped to the
    /// configured axis range and subtracted from the accumulator. Returns
    /// `None` once nothing is pending, so `Overflow::Burst` can loop on it.
    pub fn take_mouse_frame(&mut self) -> Option<Report> {
        if !self.mouse_pending() {
            return None;
        }
        self.mouse_dirty = false;
        let max = self.axis_bits.max();
        let x = self.accum.x.clamp(-max, max);
        let y = self.accum.y.clamp(-max, max);
        let wheel = self.accum.wheel.clamp(-127, 127);
        self.accum.x -= x;
        self.accum.y -= y;
        self.accum.wheel -= wheel;
        Some(self.mouse_report(x, y, wheel))
    }
}

/// Result of translating one event.
#[derive(Clone, Copy)]
pub enum Outcome {
    /// Nothing to send.
    Nothing,
    /// A keyboard input report.
    Keyboard(Report),
    /// A gamepad input report.
    Gamepad(Report),
    /// End of an input frame (`SYN_REPORT`): a flush point. Necessary for a
    /// flush in every batching mode, and sufficient in `Batch::None` (§7.2c).
    Sync,
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

/// The outcomes of translating one event, in the order they must be sent. More
/// than one arises when a broken chord prefix is replayed: the keys held back
/// are forwarded in press order, then the event that broke the chord (§7.3).
#[derive(Clone, Copy)]
pub struct Outcomes {
    buf: [Outcome; MAX_CHORD_KEYS + 1],
    len: usize,
}

impl Default for Outcomes {
    fn default() -> Self {
        Self {
            buf: [Outcome::Nothing; MAX_CHORD_KEYS + 1],
            len: 0,
        }
    }
}

impl Outcomes {
    /// Queue an outcome. `Nothing` is dropped, so callers can push the result
    /// of a translation step without checking it first.
    fn push(&mut self, out: Outcome) {
        if matches!(out, Outcome::Nothing) {
            return;
        }
        debug_assert!(self.len < self.buf.len(), "outcome buffer overflow");
        if self.len < self.buf.len() {
            self.buf[self.len] = out;
            self.len += 1;
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The `i`th queued outcome (`Nothing` past the end).
    pub fn get(&self, i: usize) -> Outcome {
        if i < self.len {
            self.buf[i]
        } else {
            Outcome::Nothing
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Outcome> {
        self.buf[..self.len].iter()
    }
}

impl InputState {
    pub fn reset(&mut self) {
        self.mouse_buttons = 0;
        self.modifiers = 0;
        self.pressed_keys = [0; 8];
        self.capture = true;
        self.chord.clear();
        self.chord_held.clear();
        self.touching = false;
        self.last_abs = [None; 2];
        self.accum = MouseAccum::default();
        for gp in self.gamepads.iter_mut() {
            *gp = GamepadState::neutral();
        }
    }

    /// A neutral input report for every advertised gamepad, to release any held
    /// controller state host-side (on drop/exit/capture-off).
    pub fn gamepad_neutral_reports(&self) -> Vec<Report> {
        let neutral = GamepadState::neutral();
        (0..self.gamepads.len())
            .map(|slot| Report::new(&neutral.report(slot as u8)))
            .collect()
    }

    /// Build a mouse report from the current button state and given axes. The
    /// axis fields are 8- or 16-bit little-endian per `[pointer] axis_bits`,
    /// matching `sdp::mouse_block` (design/ARCH.md §5).
    pub fn mouse_report(&self, x: i32, y: i32, wheel: i32) -> Report {
        let buttons = self.mouse_buttons;
        let wheel = wheel.clamp(-127, 127) as i8 as u8;
        match self.axis_bits {
            AxisBits::Eight => {
                let x = x.clamp(-127, 127) as i8 as u8;
                let y = y.clamp(-127, 127) as i8 as u8;
                Report::new(&[0xA1, 0x01, buttons, x, y, wheel])
            }
            AxisBits::Sixteen => {
                let [x_lo, x_hi] = (x.clamp(-32767, 32767) as i16).to_le_bytes();
                let [y_lo, y_hi] = (y.clamp(-32767, 32767) as i16).to_le_bytes();
                Report::new(&[0xA1, 0x01, buttons, x_lo, x_hi, y_lo, y_hi, wheel])
            }
        }
    }

    /// Build a keyboard report from the current modifier and key state.
    pub fn keyboard_report(&self) -> Report {
        let mut r = [0u8; 11];
        r[0] = 0xA1;
        r[1] = 0x02;
        r[2] = self.modifiers;
        r[3..11].copy_from_slice(&self.pressed_keys);
        Report::new(&r)
    }

    /// An all-keys-up keyboard report (modifiers 0, no keys).
    pub fn keys_up_report() -> Report {
        let mut r = [0u8; 11];
        r[0] = 0xA1;
        r[1] = 0x02;
        Report::new(&r)
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

/// Translate one raw input event, updating `state` and queuing what to transmit
/// (or the hotkey action that fired) in `out`, which is cleared first.
pub fn translate(hotkeys: &Hotkeys, state: &mut InputState, ev: RawEvent, out: &mut Outcomes) {
    out.clear();
    // Frame boundaries are checked ahead of the gamepad dispatch: every device
    // shares one channel, and a SYN from any of them is a valid flush point.
    if ev.type_ == EV_SYN {
        if ev.code == SYN_REPORT {
            out.push(Outcome::Sync);
        }
        return;
    }
    if let Some(slot) = ev.gamepad {
        out.push(translate_gamepad(state, slot, ev.type_, ev.code, ev.value));
        return;
    }
    match ev.type_ {
        EV_KEY => translate_key(hotkeys, state, ev.code, ev.value, out),
        EV_REL => out.push(translate_rel(state, ev.code, ev.value)),
        EV_ABS => out.push(translate_abs(state, ev.code, ev.value)),
        _ => {}
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
        Outcome::Gamepad(Report::new(&gp.report(slot)))
    } else {
        Outcome::Nothing
    }
}

fn translate_key(
    hotkeys: &Hotkeys,
    state: &mut InputState,
    code: u16,
    value: i32,
    out: &mut Outcomes,
) {
    use keymap::*;
    match code {
        BTN_LEFT | BTN_RIGHT | BTN_MIDDLE | BTN_TOUCH => {
            out.push(translate_mouse_key(state, code, value));
            return;
        }
        _ => {}
    }
    // Keys a fired chord consumed: swallow everything until they are released,
    // since the host never saw them go down.
    if state.chord_held.contains(code) {
        if value != 1 {
            if value == 0 {
                state.chord_held.remove(code);
            }
            return;
        }
        state.chord_held.remove(code);
    }
    if !state.chord.is_empty() {
        // A press that keeps some chord alive extends the buffer; a chord that
        // completes fires and consumes every key it swallowed. Anything else —
        // an unrelated press, or any release or autorepeat — settles it: no
        // chord is coming, so replay what was held back, in press order.
        if value == 1 && chord_advance(hotkeys, state, code) {
            if let Some(action) = chord_fire(hotkeys, state) {
                state.chord_held.push(code);
                out.push(apply_action(state, action));
            }
            return;
        }
        chord_replay(state, out);
    }
    // A key that starts some chord is held back rather than forwarded; a
    // single-key chord is complete at once.
    if value == 1 && chord_open(hotkeys, state, code) {
        if let Some(action) = chord_fire(hotkeys, state) {
            out.push(apply_action(state, action));
        }
        return;
    }
    out.push(plain_key(state, code, value));
}

/// Open a chord buffer on `code` if it starts any chord. `false` if none does,
/// leaving the buffer untouched and the key to be forwarded as usual.
fn chord_open(hotkeys: &Hotkeys, state: &mut InputState, code: u16) -> bool {
    if !hotkeys.starts(code) {
        return false;
    }
    for (i, chord) in hotkeys.chords().iter().enumerate() {
        if chord.steps[0].matches(code) {
            state.chord.push_cand(i as u8, 1);
        }
    }
    state.chord.keys.push(code);
    true
}

/// Fold a further press into the live buffer, dropping the candidates it rules
/// out. `false` if no candidate accepts `code`, i.e. the chord is broken.
fn chord_advance(hotkeys: &Hotkeys, state: &mut InputState, code: u16) -> bool {
    let mut kept = 0;
    for i in 0..state.chord.n_cands as usize {
        let cand = state.chord.cands[i];
        let steps = &hotkeys.chords()[cand.chord as usize].steps;
        // Only the first key of a chord is ordered; the rest may arrive in any
        // order, so any not-yet-matched step will do.
        let Some(step) =
            (1..steps.len()).find(|&s| cand.matched & (1 << s) == 0 && steps[s].matches(code))
        else {
            continue;
        };
        state.chord.cands[kept] = Cand {
            chord: cand.chord,
            matched: cand.matched | (1 << step),
        };
        kept += 1;
    }
    state.chord.n_cands = kept as u8;
    if kept > 0 {
        state.chord.keys.push(code);
    }
    kept > 0
}

/// The action of the completed chord, if the buffer now satisfies one. The
/// longest match wins (ties go to config order), and firing consumes the whole
/// buffer: its keys move to `chord_held` so their releases stay local.
fn chord_fire(hotkeys: &Hotkeys, state: &mut InputState) -> Option<Action> {
    let chords = hotkeys.chords();
    let complete = |c: &Cand| -> Option<&Chord> {
        let chord = &chords[c.chord as usize];
        let all = (1u8 << chord.steps.len()) - 1;
        (c.matched == all).then_some(chord)
    };
    let action = state
        .chord
        .cands()
        .iter()
        .filter_map(complete)
        .max_by_key(|chord| chord.steps.len())?
        .action;
    for &code in state.chord.keys.as_slice() {
        state.chord_held.push(code);
    }
    state.chord.clear();
    Some(action)
}

/// Give up on the buffered prefix: forward the keys held back, in press order,
/// as if they had never been delayed.
fn chord_replay(state: &mut InputState, out: &mut Outcomes) {
    let keys = state.chord.keys;
    state.chord.clear();
    for &code in keys.as_slice() {
        out.push(plain_key(state, code, 1));
    }
}

/// Apply a fired hotkey to the session state and report what it did.
fn apply_action(state: &mut InputState, action: Action) -> Outcome {
    match action {
        Action::Exit => Outcome::Exit,
        Action::DropConnection => Outcome::DropSession,
        Action::CaptureOn => {
            state.capture = true;
            Outcome::CaptureOn
        }
        Action::CaptureOff => {
            state.capture = false;
            Outcome::CaptureOff
        }
        Action::CaptureToggle => {
            state.capture = !state.capture;
            if state.capture {
                Outcome::CaptureOn
            } else {
                Outcome::CaptureOff
            }
        }
    }
}

/// Track a key that is not part of any chord in progress and, while capturing,
/// emit the resulting keyboard report.
fn plain_key(state: &mut InputState, code: u16, value: i32) -> Outcome {
    use keymap::*;
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

/// Mouse buttons and touchpad contact. Neither takes part in chords, so they
/// never disturb a buffered prefix.
fn translate_mouse_key(state: &mut InputState, code: u16, value: i32) -> Outcome {
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
            // Like motion, a button change waits for the frame boundary, so a
            // click and the motion alongside it become one report and neither
            // can overtake the other (§7.2c).
            if state.capture {
                state.mouse_dirty = true;
            }
            Outcome::Nothing
        }
        _ => {
            state.touching = value != 0;
            state.last_abs = [None; 2];
            Outcome::Nothing
        }
    }
}

/// Accumulate relative motion. Nothing is emitted here: a hardware frame
/// carries one event per axis, so emitting per event would cost two reports for
/// a plain diagonal move. The accumulator is drained at the next `SYN_REPORT`
/// (design/ARCH.md §7.2c).
fn translate_rel(state: &mut InputState, code: u16, value: i32) -> Outcome {
    if !state.capture {
        return Outcome::Nothing;
    }
    match code {
        keymap::REL_X => state.accum.x += value,
        keymap::REL_Y => state.accum.y += value,
        keymap::REL_WHEEL => state.accum.wheel += value,
        _ => {}
    }
    Outcome::Nothing
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
    // Same accumulate-and-drain-at-SYN treatment as EV_REL (§7.2c).
    let d = value - prev;
    if axis == 0 {
        state.accum.x += d;
    } else {
        state.accum.y += d;
    }
    Outcome::Nothing
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

    fn rel(code: u16, value: i32) -> RawEvent {
        RawEvent {
            type_: EV_REL,
            code,
            value,
            gamepad: None,
        }
    }

    /// End of an input frame — the flush point every batching mode needs.
    fn syn() -> RawEvent {
        RawEvent {
            type_: EV_SYN,
            code: SYN_REPORT,
            value: 0,
            gamepad: None,
        }
    }

    /// Translate one event and collect everything it produced, in order. Most
    /// events yield one outcome; a broken chord prefix yields several.
    fn tr(hk: &Hotkeys, s: &mut InputState, ev: RawEvent) -> Vec<Outcome> {
        let mut out = Outcomes::default();
        translate(hk, s, ev, &mut out);
        out.iter().copied().collect()
    }

    /// The single outcome of an event, or `Nothing` if it produced none.
    fn one(hk: &Hotkeys, s: &mut InputState, ev: RawEvent) -> Outcome {
        let outs = tr(hk, s, ev);
        assert!(outs.len() <= 1, "expected at most one outcome");
        outs.first().copied().unwrap_or(Outcome::Nothing)
    }

    /// The modifier byte of a keyboard outcome.
    fn mods(out: &Outcome) -> u8 {
        match out {
            Outcome::Keyboard(r) => r[2],
            _ => panic!("expected a keyboard report"),
        }
    }

    #[test]
    fn press_release_key() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Press 'a' → usage 4 in slot 0.
        match one(&hk, &mut s, key(keymap::KEY_A, 1)) {
            Outcome::Keyboard(r) => assert_eq!(&r[..5], &[0xA1, 0x02, 0x00, 4, 0]),
            _ => panic!(),
        }
        // Press 'b' → usage 5 appended.
        tr(&hk, &mut s, key(keymap::KEY_B, 1));
        assert_eq!(&s.pressed_keys[..2], &[4, 5]);
        // Release 'a' → shift left.
        tr(&hk, &mut s, key(keymap::KEY_A, 0));
        assert_eq!(&s.pressed_keys[..2], &[5, 0]);
    }

    #[test]
    fn autorepeat_no_change() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        tr(&hk, &mut s, key(keymap::KEY_A, 1));
        tr(&hk, &mut s, key(keymap::KEY_A, 2));
        assert_eq!(&s.pressed_keys[..2], &[4, 0]);
    }

    /// Right Shift completes the default chords but starts none of them, so it
    /// is forwarded like any other modifier — the host must see it.
    #[test]
    fn bare_trigger_key_is_forwarded() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        assert_eq!(
            mods(&one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1))),
            0x20
        );
        // Shifted letter reaches the host with the modifier set.
        assert_eq!(
            mods(&one(&hk, &mut s, key(keymap::KEY_A, 1))),
            0x20,
            "Right Shift + 'a' must arrive shifted"
        );
        tr(&hk, &mut s, key(keymap::KEY_A, 0));
        assert_eq!(
            mods(&one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0))),
            0x00
        );
    }

    /// A chord prefix is held back, then replayed in press order once it is
    /// clear no chord is coming.
    #[test]
    fn broken_prefix_is_replayed_in_order() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Left Shift starts the capture toggle: nothing goes out yet.
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1)).is_empty());
        assert_eq!(s.modifiers, 0x00);
        // 'a' cannot continue it: the shift is replayed, then 'a' follows.
        let outs = tr(&hk, &mut s, key(keymap::KEY_A, 1));
        assert_eq!(outs.len(), 2);
        assert_eq!(mods(&outs[0]), 0x02);
        match outs[1] {
            Outcome::Keyboard(r) => assert_eq!(&r[..5], &[0xA1, 0x02, 0x02, 4, 0]),
            _ => panic!("expected the shifted 'a'"),
        }
        // Releasing a lone prefix key replays it too, then releases it.
        s.reset();
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 1)).is_empty());
        let outs = tr(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 0));
        assert_eq!(outs.len(), 2);
        assert_eq!(mods(&outs[0]), 0x01);
        assert_eq!(mods(&outs[1]), 0x00);
        assert_eq!(s.modifiers, 0x00);
    }

    /// Only the first key of a chord is ordered; the rest may come in any order.
    #[test]
    fn rest_of_chord_is_unordered() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Left Ctrl first, then Right Shift before Left Alt.
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 1)).is_empty());
        assert!(tr(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)).is_empty());
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_LEFTALT, 1)),
            Outcome::Exit
        ));
        // Starting with a key that is not first, though, forwards it: Right
        // Shift then Left Shift is not the capture toggle.
        s.reset();
        assert_eq!(
            mods(&one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1))),
            0x20
        );
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1)).is_empty());
        let outs = tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 0));
        assert_eq!(outs.len(), 2);
        assert_eq!(mods(&outs[0]), 0x22);
        assert_eq!(mods(&outs[1]), 0x20);
    }

    /// A fired chord forwards none of its keys, and swallows their releases so
    /// the host is never left with a stuck modifier.
    #[test]
    fn fired_chord_forwards_nothing() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1)).is_empty());
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)),
            Outcome::CaptureOff
        ));
        assert!(tr(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0)).is_empty());
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 0)).is_empty());
        assert_eq!(s.modifiers, 0x00);
        // Once released, those keys forward normally again.
        s.capture = true;
        assert_eq!(
            mods(&one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1))),
            0x20
        );
    }

    /// Pointer activity passes through without disturbing a buffered prefix.
    #[test]
    fn motion_leaves_the_buffer_alone() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        assert!(tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1)).is_empty());
        tr(&hk, &mut s, rel(keymap::REL_X, 5));
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)),
            Outcome::CaptureOff
        ));
    }

    #[test]
    fn capture_toggle() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        // Left Shift then Right Shift → capture off.
        tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1));
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)),
            Outcome::CaptureOff
        ));
        tr(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0));
        tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 0));
        assert!(!s.capture);
        // While off: keys and motion are tracked but not forwarded.
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_A, 1)),
            Outcome::Nothing
        ));
        assert_eq!(s.pressed_keys[0], 4);
        assert!(matches!(
            one(&hk, &mut s, rel(keymap::REL_X, 5)),
            Outcome::Nothing
        ));
        // Exit hotkey still works while off.
        tr(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 1));
        tr(&hk, &mut s, key(keymap::KEY_LEFTALT, 1));
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)),
            Outcome::Exit
        ));
        tr(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 0));
        tr(&hk, &mut s, key(keymap::KEY_LEFTCTRL, 0));
        tr(&hk, &mut s, key(keymap::KEY_LEFTALT, 0));
        // Left Shift then Right Shift again → capture back on.
        tr(&hk, &mut s, key(keymap::KEY_LEFTSHIFT, 1));
        assert!(matches!(
            one(&hk, &mut s, key(keymap::KEY_RIGHTSHIFT, 1)),
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
            one(&hk, &mut s, abs(keymap::ABS_X, 500)),
            Outcome::Nothing
        ));
        // Finger down: first position only seeds the reference.
        one(&hk, &mut s, key(keymap::BTN_TOUCH, 1));
        assert!(matches!(
            one(&hk, &mut s, abs(keymap::ABS_X, 1000)),
            Outcome::Nothing
        ));
        assert!(matches!(
            one(&hk, &mut s, abs(keymap::ABS_Y, 800)),
            Outcome::Nothing
        ));
        // Subsequent positions accumulate relative motion; both axes of the
        // frame land in the one report the SYN drains.
        assert!(matches!(
            one(&hk, &mut s, abs(keymap::ABS_X, 1010)),
            Outcome::Nothing
        ));
        assert!(matches!(
            one(&hk, &mut s, abs(keymap::ABS_Y, 795)),
            Outcome::Nothing
        ));
        assert!(matches!(one(&hk, &mut s, syn()), Outcome::Sync));
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 10, -5i8 as u8, 0]
        );
        assert!(s.take_mouse_frame().is_none());
        // Lifting resets the reference: no jump on the next touch.
        one(&hk, &mut s, key(keymap::BTN_TOUCH, 0));
        one(&hk, &mut s, key(keymap::BTN_TOUCH, 1));
        one(&hk, &mut s, abs(keymap::ABS_X, 2000));
        assert!(!s.mouse_pending());
    }

    /// One hardware frame carrying both axes must cost exactly one report, not
    /// one per axis (design/ARCH.md §7.2c).
    #[test]
    fn rel_frame_is_one_report() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        assert!(matches!(
            one(&hk, &mut s, rel(keymap::REL_X, 3)),
            Outcome::Nothing
        ));
        assert!(matches!(
            one(&hk, &mut s, rel(keymap::REL_Y, -4)),
            Outcome::Nothing
        ));
        assert!(matches!(one(&hk, &mut s, syn()), Outcome::Sync));
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 3, -4i8 as u8, 0]
        );
        assert!(s.take_mouse_frame().is_none());
    }

    /// Motion beyond one report's range is handed out a report at a time, so
    /// `Overflow::Burst` can drain it losslessly.
    #[test]
    fn mouse_rel_saturates_a_report_at_a_time() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        one(&hk, &mut s, rel(keymap::REL_X, 300));
        one(&hk, &mut s, syn());
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 127, 0, 0]
        );
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 127, 0, 0]
        );
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 46, 0, 0]
        );
        assert!(s.take_mouse_frame().is_none());
    }

    /// With 16-bit axes the same motion fits in one 8-byte report.
    #[test]
    fn mouse_rel_16_bit() {
        let hk = Hotkeys::default();
        let mut s = InputState::default().with_pointer(AxisBits::Sixteen, Overflow::Burst);
        one(&hk, &mut s, rel(keymap::REL_X, 300));
        one(&hk, &mut s, rel(keymap::REL_Y, -300));
        one(&hk, &mut s, syn());
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 0x2C, 0x01, 0xD4, 0xFE, 0]
        );
        assert!(s.take_mouse_frame().is_none());
    }

    /// A click and the motion alongside it are one report, and a click with no
    /// motion still produces one.
    #[test]
    fn button_change_joins_the_frame() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        one(&hk, &mut s, rel(keymap::REL_X, 2));
        assert!(matches!(
            one(&hk, &mut s, key(keymap::BTN_LEFT, 1)),
            Outcome::Nothing
        ));
        one(&hk, &mut s, syn());
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x01, 2, 0, 0]
        );
        // Release with no motion: still a report, so the host sees the button up.
        one(&hk, &mut s, key(keymap::BTN_LEFT, 0));
        one(&hk, &mut s, syn());
        assert_eq!(
            s.take_mouse_frame().unwrap().as_slice(),
            [0xA1, 0x01, 0x00, 0, 0, 0]
        );
        assert!(s.take_mouse_frame().is_none());
    }

    /// Pending motion must not survive a capture pause or a reconnect (§6.3).
    #[test]
    fn pending_motion_is_discarded_on_reset() {
        let hk = Hotkeys::default();
        let mut s = InputState::default();
        one(&hk, &mut s, rel(keymap::REL_X, 40));
        assert!(s.mouse_pending());
        s.clear_mouse();
        assert!(!s.mouse_pending());

        one(&hk, &mut s, rel(keymap::REL_X, 40));
        s.reset();
        assert!(!s.mouse_pending());
    }

    /// Motion arriving while capture is off is dropped, not banked up.
    #[test]
    fn no_accumulation_while_capture_off() {
        let hk = Hotkeys::default();
        let mut s = InputState {
            capture: false,
            ..Default::default()
        };
        one(&hk, &mut s, rel(keymap::REL_X, 40));
        one(&hk, &mut s, syn());
        assert!(!s.mouse_pending());
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
            one(&hk, &mut s, key(code, 1));
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
        match one(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 1)) {
            // [header, id, b_lo, b_hi, hat, lx, ly, rx, ry, lt, rt]
            Outcome::Gamepad(r) => {
                assert_eq!(r[..5], [0xA1, 0x03, 0x01, 0x00, 8]);
                // Sticks neutral (128), triggers 0.
                assert_eq!(r[5..], [128, 128, 128, 128, 0, 0]);
            }
            _ => panic!(),
        }
        // Left stick X (already normalized) on slot 0.
        match one(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_X, 200)) {
            Outcome::Gamepad(r) => assert_eq!(r[5], 200),
            _ => panic!(),
        }
        // Release the button.
        match one(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 0)) {
            Outcome::Gamepad(r) => assert_eq!(r[2], 0x00),
            _ => panic!(),
        }
        // Slot 1 is independent and uses report ID 4.
        match one(&hk, &mut s, gp(1, EV_KEY, keymap::BTN_START, 1)) {
            Outcome::Gamepad(r) => assert_eq!(r[..4], [0xA1, 0x04, 0x00, 0x08]),
            _ => panic!(),
        }
    }

    #[test]
    fn gamepad_hat_encoding() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(1);
        // Up-left → NW (7).
        one(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0X, -1));
        match one(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0Y, -1)) {
            Outcome::Gamepad(r) => assert_eq!(r[4], 7),
            _ => panic!(),
        }
        // Back to centre → 8.
        one(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0X, 0));
        match one(&hk, &mut s, gp(0, EV_ABS, keymap::ABS_HAT0Y, 0)) {
            Outcome::Gamepad(r) => assert_eq!(r[4], 8),
            _ => panic!(),
        }
    }

    #[test]
    fn gamepad_out_of_range_slot_ignored() {
        let hk = Hotkeys::default();
        let mut s = InputState::with_gamepads(1);
        assert!(matches!(
            one(&hk, &mut s, gp(5, EV_KEY, keymap::BTN_SOUTH, 1)),
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
            one(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_SOUTH, 1)),
            Outcome::Nothing
        ));
        s.capture = true;
        // The tracked press is reflected once capturing resumes.
        match one(&hk, &mut s, gp(0, EV_KEY, keymap::BTN_EAST, 1)) {
            Outcome::Gamepad(r) => assert_eq!(r[2], 0b11), // south + east
            _ => panic!(),
        }
    }
}
