//! TOML-based configuration: the local hotkey chords. See config.example.toml.

use std::path::{Path, PathBuf};

use toml_spanner::{Context, Failed, FromToml, Item};

use crate::keymap;

/// What a hotkey chord does when it fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    DropConnection,
    Exit,
    CaptureOn,
    CaptureOff,
    CaptureToggle,
}

/// A parsed hotkey: modifiers that must be held when `trigger` is released.
#[derive(Clone, Debug)]
pub struct Chord {
    pub trigger: u16,
    /// One HID-modifier bitmask per listed modifier; each must intersect the
    /// currently held modifiers (side-agnostic aliases set both side bits).
    pub mod_masks: Vec<u8>,
    pub action: Action,
}

/// The full hotkey table. Falls back to the built-in scroll-lock defaults for
/// anything the config file does not override.
#[derive(Clone, Debug)]
pub struct Hotkeys {
    chords: Vec<Chord>,
}

/// How many gamepad controllers to advertise (each gets its own HID report ID).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GamepadSlots {
    /// Advertise one controller per gamepad present at startup (the default).
    #[default]
    Initial,
    /// Advertise a fixed number of controllers (`0` disables gamepad forwarding).
    Fixed(usize),
}

/// Whether to hotplug gamepads into free advertised slots at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Hotplug {
    /// On iff `slots` is a fixed count greater than zero (the default).
    #[default]
    Auto,
    On,
    Off,
}

impl Hotplug {
    /// Resolve `Auto` against the configured slot policy: hotplug is worthwhile
    /// only when there are fixed spare slots to fill (`Fixed(n)`, `n > 0`).
    pub fn enabled(self, slots: GamepadSlots) -> bool {
        match self {
            Hotplug::On => true,
            Hotplug::Off => false,
            Hotplug::Auto => matches!(slots, GamepadSlots::Fixed(n) if n > 0),
        }
    }
}

/// Which Bluetooth transport blooter presents itself over. BLE (HID-over-GATT,
/// HOGP) is the default; Classic is BR/EDR HID. See design/ARCH.md §4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Protocol {
    /// Bluetooth Classic (BR/EDR) HID.
    Classic,
    /// Bluetooth Low Energy (HID-over-GATT / HOGP) — the default.
    #[default]
    Ble,
}

/// How the pairing agent handles bonding requests. See design/CONNECTION.md §5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PairingMode {
    /// Accept every request silently ("Just Works") — the default.
    #[default]
    Accept,
    /// Prompt on the TTY if there is one, else accept silently.
    PromptIfPossible,
    /// Prompt the user on the TTY before bonding. Errors at startup when stdin
    /// is not a TTY, since there is nothing to prompt on.
    Prompt,
}

/// What drives a flush of buffered pointer motion (design/ARCH.md §7.2c). A
/// `SYN_REPORT` frame boundary is necessary for a flush in every mode, and
/// sufficient in `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Batch {
    /// A per-transport minimum spacing between pointer reports (the default):
    /// 8 ms on Classic, 15 ms on BLE.
    #[default]
    Auto,
    /// No timer: coalesce whatever piled up while the previous send was in
    /// flight. Genuinely backpressure-driven on Classic, opportunistic on BLE.
    Adaptive,
    /// No buffering: flush on every frame boundary.
    None,
    /// An explicit minimum spacing in milliseconds, overriding `Auto`.
    Millis(u64),
}

impl Batch {
    /// The minimum spacing between flushes, given the transport's `auto` pick.
    /// `None` for the untimed modes.
    pub fn interval(self, auto: std::time::Duration) -> Option<std::time::Duration> {
        match self {
            Batch::Auto => Some(auto),
            Batch::Millis(ms) => Some(std::time::Duration::from_millis(ms)),
            Batch::Adaptive | Batch::None => None,
        }
    }
}

/// The width of the relative X/Y axes in the HID report descriptor. Widening
/// them lets merged motion accumulate without saturating, at the cost of a
/// descriptor change (design/ARCH.md §3.2, §7.2c).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AxisBits {
    /// Signed 8-bit, −127..=127 — the compatible default.
    #[default]
    Eight,
    /// Signed 16-bit, −32767..=32767.
    Sixteen,
}

impl AxisBits {
    /// The largest magnitude one report can carry on X/Y.
    pub fn max(self) -> i32 {
        match self {
            AxisBits::Eight => 127,
            AxisBits::Sixteen => 32767,
        }
    }
}

/// What to do when accumulated motion exceeds what one report can carry
/// (design/ARCH.md §7.2c).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Emit as many back-to-back reports as it takes (the default). Lossless,
    /// and bounded by the outgoing buffer.
    #[default]
    Burst,
    /// Emit one saturated report and keep the remainder for the next frame.
    Carry,
    /// Emit one saturated report and discard the remainder.
    Clamp,
}

/// Outgoing report slots per connection when `[pointer] buffer` is unset.
pub const DEFAULT_BUFFER: usize = 16;

/// The parsed configuration file.
#[derive(Clone, Debug)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub gamepad_slots: GamepadSlots,
    pub hotplug: Hotplug,
    pub protocol: Protocol,
    /// Pairing agent behaviour (§5).
    pub pairing: PairingMode,
    /// Address of a host to initiate an outgoing HID connection to (§3.2, §6).
    pub reconnect: Option<String>,
    /// Pointer batching (§7.2c).
    pub batch: Batch,
    pub buffer: usize,
    pub axis_bits: AxisBits,
    pub overflow: Overflow,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkeys: Hotkeys::default(),
            gamepad_slots: GamepadSlots::default(),
            hotplug: Hotplug::default(),
            protocol: Protocol::default(),
            pairing: PairingMode::default(),
            reconnect: None,
            batch: Batch::default(),
            buffer: DEFAULT_BUFFER,
            axis_bits: AxisBits::default(),
            overflow: Overflow::default(),
        }
    }
}

/// The recognized `[hotkeys]` keys and their built-in defaults. An empty
/// string means the hotkey is disabled.
const DEFAULTS: [(&str, &str); 5] = [
    ("drop_connection", ""),
    ("exit", "leftcontrol+leftalt+rightshift"),
    ("capture_toggle", "leftshift+rightshift"),
    ("capture_on", ""),
    ("capture_off", ""),
];

fn action_for(key: &str) -> Action {
    match key {
        "drop_connection" => Action::DropConnection,
        "exit" => Action::Exit,
        "capture_toggle" => Action::CaptureToggle,
        "capture_on" => Action::CaptureOn,
        "capture_off" => Action::CaptureOff,
        _ => unreachable!("unrecognized hotkey key"),
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        let chords = DEFAULTS
            .iter()
            .filter(|(_, spec)| !spec.is_empty())
            .map(|(key, spec)| {
                let (trigger, mod_masks) =
                    parse_chord_spec(spec).expect("built-in default chords parse");
                Chord {
                    trigger,
                    mod_masks,
                    action: action_for(key),
                }
            })
            .collect();
        Hotkeys { chords }
    }
}

impl Hotkeys {
    /// Whether `code` is the trigger key of any configured chord. Trigger keys
    /// are consumed locally and never forwarded to the host.
    pub fn is_trigger(&self, code: u16) -> bool {
        self.chords.iter().any(|c| c.trigger == code)
    }

    /// The action to fire when `code` is released with `modifiers` held, if
    /// any. The most specific matching chord (most modifiers) wins.
    pub fn action(&self, code: u16, modifiers: u8) -> Option<Action> {
        self.chords
            .iter()
            .filter(|c| c.trigger == code && c.mod_masks.iter().all(|m| modifiers & m != 0))
            .max_by_key(|c| c.mod_masks.len())
            .map(|c| c.action)
    }
}

/// Read and parse a config file.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The first existing default config location:
/// `$XDG_CONFIG_HOME/blooter/config.toml` (or `~/.config/blooter/config.toml`),
/// then `/etc/blooter/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => candidates.push(PathBuf::from(x).join("blooter/config.toml")),
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                candidates.push(PathBuf::from(home).join(".config/blooter/config.toml"));
            }
        }
    }
    candidates.push(PathBuf::from("/etc/blooter/config.toml"));
    candidates.into_iter().find(|p| p.exists())
}

/// Parse config text into the full configuration.
pub fn parse(text: &str) -> Result<Config, String> {
    match toml_spanner::from_str::<TopLevel>(text) {
        Ok(top) => {
            let gamepad = top.gamepad.unwrap_or_default();
            let connection = top.connection.unwrap_or_default();
            let pointer = top.pointer.unwrap_or_default();
            Ok(Config {
                hotkeys: top.hotkeys.unwrap_or_default(),
                gamepad_slots: gamepad.slots,
                hotplug: gamepad.hotplug,
                protocol: connection.protocol,
                pairing: connection.pairing,
                reconnect: connection.reconnect,
                batch: pointer.batch,
                buffer: pointer.buffer,
                axis_bits: pointer.axis_bits,
                overflow: pointer.overflow,
            })
        }
        Err(e) => {
            let msgs: Vec<String> = e
                .into_iter()
                .map(|err| {
                    let upto = (err.span().start as usize).min(text.len());
                    let line = text[..upto].matches('\n').count() + 1;
                    format!("line {line}: {err}")
                })
                .collect();
            Err(msgs.join("; "))
        }
    }
}

/// The whole config file: the optional `[hotkeys]`, `[gamepad]`, `[connection]`
/// and `[pointer]` tables.
struct TopLevel {
    hotkeys: Option<Hotkeys>,
    gamepad: Option<Gamepad>,
    connection: Option<Connection>,
    pointer: Option<Pointer>,
}

impl<'de> FromToml<'de> for TopLevel {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let hotkeys = th.optional("hotkeys");
        let gamepad = th.optional("gamepad");
        let connection = th.optional("connection");
        let pointer = th.optional("pointer");
        th.require_empty()?;
        Ok(TopLevel {
            hotkeys,
            gamepad,
            connection,
            pointer,
        })
    }
}

/// The `[pointer]` table (design/ARCH.md §7.2c).
struct Pointer {
    batch: Batch,
    buffer: usize,
    axis_bits: AxisBits,
    overflow: Overflow,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            batch: Batch::default(),
            buffer: DEFAULT_BUFFER,
            axis_bits: AxisBits::default(),
            overflow: Overflow::default(),
        }
    }
}

impl<'de> FromToml<'de> for Pointer {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let batch = th.optional_mapped("batch", batch_item).unwrap_or_default();
        let buffer = th
            .optional_mapped("buffer", buffer_item)
            .unwrap_or(DEFAULT_BUFFER);
        let axis_bits = th
            .optional_mapped("axis_bits", axis_bits_item)
            .unwrap_or_default();
        let overflow = th
            .optional_mapped("overflow", overflow_item)
            .unwrap_or_default();
        th.require_empty()?;
        Ok(Pointer {
            batch,
            buffer,
            axis_bits,
            overflow,
        })
    }
}

/// Parse the `batch` value: `"auto"`, `"adaptive"`, `"none"`, or milliseconds.
fn batch_item(item: &Item<'_>) -> Result<Batch, toml_spanner::Error> {
    match item.as_str() {
        Some("auto") => return Ok(Batch::Auto),
        Some("adaptive") => return Ok(Batch::Adaptive),
        Some("none") => return Ok(Batch::None),
        Some(_) => {}
        None => {
            if let Some(ms) = item.as_u64() {
                // 0 ms is not "no spacing but still buffered" — that is `"none"`.
                return Ok(if ms == 0 {
                    Batch::None
                } else {
                    Batch::Millis(ms)
                });
            }
        }
    }
    Err(item.expected(&"\"auto\", \"adaptive\", \"none\", or a millisecond count"))
}

/// Parse the `buffer` value: how many outgoing report slots to hold.
fn buffer_item(item: &Item<'_>) -> Result<usize, toml_spanner::Error> {
    match item.as_u64() {
        Some(n) if n >= 1 => Ok(n as usize),
        _ => Err(item.expected(&"an integer of at least 1")),
    }
}

/// Parse the `axis_bits` value: the integer 8 or 16.
fn axis_bits_item(item: &Item<'_>) -> Result<AxisBits, toml_spanner::Error> {
    match item.as_u64() {
        Some(8) => Ok(AxisBits::Eight),
        Some(16) => Ok(AxisBits::Sixteen),
        _ => Err(item.expected(&"8 or 16")),
    }
}

/// Parse the `overflow` value: `"burst"`, `"carry"` or `"clamp"`.
fn overflow_item(item: &Item<'_>) -> Result<Overflow, toml_spanner::Error> {
    match item.as_str() {
        Some("burst") => Ok(Overflow::Burst),
        Some("carry") => Ok(Overflow::Carry),
        Some("clamp") => Ok(Overflow::Clamp),
        _ => Err(item.expected(&"\"burst\", \"carry\" or \"clamp\"")),
    }
}

/// The `[connection]` table.
#[derive(Default)]
struct Connection {
    protocol: Protocol,
    pairing: PairingMode,
    reconnect: Option<String>,
}

impl<'de> FromToml<'de> for Connection {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let protocol = th
            .optional_mapped("protocol", protocol_item)
            .unwrap_or_default();
        let pairing = th
            .optional_mapped("pairing", pairing_item)
            .unwrap_or_default();
        let reconnect = th.optional_mapped("reconnect", reconnect_item);
        th.require_empty()?;
        Ok(Connection {
            protocol,
            pairing,
            reconnect,
        })
    }
}

/// Parse the `protocol` value: one of the strings `"classic"` or `"ble"`.
fn protocol_item(item: &Item<'_>) -> Result<Protocol, toml_spanner::Error> {
    match item.as_str() {
        Some("classic") => Ok(Protocol::Classic),
        Some("ble") => Ok(Protocol::Ble),
        _ => Err(item.expected(&"\"classic\" or \"ble\"")),
    }
}

/// Parse the `pairing` value: `"accept"`, `"prompt_if_possible"` or `"prompt"`.
fn pairing_item(item: &Item<'_>) -> Result<PairingMode, toml_spanner::Error> {
    match item.as_str() {
        Some("accept") => Ok(PairingMode::Accept),
        Some("prompt_if_possible") => Ok(PairingMode::PromptIfPossible),
        Some("prompt") => Ok(PairingMode::Prompt),
        _ => Err(item.expected(&"\"accept\", \"prompt_if_possible\" or \"prompt\"")),
    }
}

/// Parse the `reconnect` value: a Bluetooth address `"AA:BB:CC:DD:EE:FF"`.
fn reconnect_item(item: &Item<'_>) -> Result<String, toml_spanner::Error> {
    let s = item.as_str().ok_or_else(|| item.expected(&"a string"))?;
    if is_bt_address(s) {
        Ok(s.to_string())
    } else {
        Err(item.expected(&"a Bluetooth address \"AA:BB:CC:DD:EE:FF\""))
    }
}

/// Whether `s` is six colon-separated hex byte pairs.
fn is_bt_address(s: &str) -> bool {
    let mut parts = 0;
    for part in s.split(':') {
        parts += 1;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts == 6
}

/// The `[gamepad]` table.
#[derive(Default)]
struct Gamepad {
    slots: GamepadSlots,
    hotplug: Hotplug,
}

impl<'de> FromToml<'de> for Gamepad {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let slots = th.optional_mapped("slots", slots_item).unwrap_or_default();
        let hotplug = th
            .optional_mapped("hotplug", hotplug_item)
            .unwrap_or_default();
        th.require_empty()?;
        Ok(Gamepad { slots, hotplug })
    }
}

/// Parse the `hotplug` value: one of the strings `"auto"`, `"on"` or `"off"`.
fn hotplug_item(item: &Item<'_>) -> Result<Hotplug, toml_spanner::Error> {
    match item.as_str() {
        Some("auto") => Ok(Hotplug::Auto),
        Some("on") => Ok(Hotplug::On),
        Some("off") => Ok(Hotplug::Off),
        _ => Err(item.expected(&"\"auto\", \"on\" or \"off\"")),
    }
}

/// Parse the `slots` value: a non-negative integer, or the string `"initial"`.
fn slots_item(item: &Item<'_>) -> Result<GamepadSlots, toml_spanner::Error> {
    if let Some(s) = item.as_str() {
        if s == "initial" {
            return Ok(GamepadSlots::Initial);
        }
    } else if let Some(n) = item.as_u64() {
        return Ok(GamepadSlots::Fixed(n as usize));
    }
    Err(item.expected(&"a non-negative integer or \"initial\""))
}

impl<'de> FromToml<'de> for Hotkeys {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let mut chords = Vec::new();
        for (key, default) in DEFAULTS {
            match th.optional_mapped(key, chord_item) {
                Some(Some((trigger, mod_masks))) => {
                    chords.push(Chord {
                        trigger,
                        mod_masks,
                        action: action_for(key),
                    });
                }
                Some(None) => {} // explicitly disabled with ""
                None => {
                    // Absent (or invalid, in which case the error is already
                    // recorded and the parse fails): fall back to the default.
                    if !default.is_empty() {
                        let (trigger, mod_masks) =
                            parse_chord_spec(default).expect("built-in default chords parse");
                        chords.push(Chord {
                            trigger,
                            mod_masks,
                            action: action_for(key),
                        });
                    }
                }
            }
        }
        th.require_empty()?;
        Ok(Hotkeys { chords })
    }
}

/// Extract one hotkey value: a chord string, or `""` (`None`) for disabled.
fn chord_item(item: &Item<'_>) -> Result<Option<(u16, Vec<u8>)>, toml_spanner::Error> {
    let spec = item.as_str().ok_or_else(|| item.expected(&"a string"))?;
    if spec.is_empty() {
        return Ok(None);
    }
    parse_chord_spec(spec)
        .map(Some)
        .map_err(|e| toml_spanner::Error::custom_at(e, item))
}

/// Parse a chord spec: zero or more modifier names and a final trigger key,
/// joined with '+'. Key names follow keyd's keycode table. Returns the
/// trigger keycode and the required-modifier masks.
fn parse_chord_spec(spec: &str) -> Result<(u16, Vec<u8>), String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    let (&trigger_name, mod_names) = parts.split_last().expect("split yields at least one part");
    let trigger = keymap::keycode_from_name(trigger_name)
        .ok_or_else(|| format!("unknown key name '{trigger_name}'"))?;
    let mut mod_masks = Vec::new();
    for name in mod_names {
        mod_masks
            .push(modifier_mask(name).ok_or_else(|| format!("'{name}' is not a modifier key"))?);
    }
    Ok((trigger, mod_masks))
}

/// The HID-modifier bitmask a chord-modifier name requires. Side-specific
/// names set one bit; the side-agnostic aliases set both, and match when
/// either side is held.
fn modifier_mask(name: &str) -> Option<u8> {
    Some(match name {
        "control" | "ctrl" => 0x11,
        "shift" => 0x22,
        "alt" => 0x44,
        "meta" | "super" => 0x88,
        _ => {
            let code = keymap::keycode_from_name(name)?;
            1u8 << keymap::modifier_bit(code)?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap;

    #[test]
    fn defaults() {
        let hk = Hotkeys::default();
        assert!(hk.is_trigger(keymap::KEY_RIGHTSHIFT));
        assert!(!hk.is_trigger(keymap::KEY_A));
        // Bare Right Shift → nothing (drop_connection is disabled by default).
        assert_eq!(hk.action(keymap::KEY_RIGHTSHIFT, 0), None);
        // Left Ctrl+Alt held → exit wins (most specific).
        assert_eq!(hk.action(keymap::KEY_RIGHTSHIFT, 0x05), Some(Action::Exit));
        // Left Shift held → capture toggle.
        assert_eq!(
            hk.action(keymap::KEY_RIGHTSHIFT, 0x02),
            Some(Action::CaptureToggle)
        );
    }

    #[test]
    fn example_file_is_all_defaults() {
        let text = include_str!("../config.example.toml");
        let cfg = parse(text).unwrap();
        let def = Hotkeys::default();
        assert_eq!(cfg.hotkeys.chords.len(), def.chords.len());
        assert_eq!(cfg.gamepad_slots, GamepadSlots::Initial);
        assert_eq!(cfg.hotplug, Hotplug::Auto);
        assert_eq!(cfg.protocol, Protocol::Ble);
        assert_eq!(cfg.pairing, PairingMode::Accept);
        assert_eq!(cfg.reconnect, None);
        assert_eq!(cfg.batch, Batch::Auto);
        assert_eq!(cfg.buffer, DEFAULT_BUFFER);
        assert_eq!(cfg.axis_bits, AxisBits::Eight);
        assert_eq!(cfg.overflow, Overflow::Burst);
    }

    #[test]
    fn pointer_parse() {
        // Absent → defaults, both with and without the table present.
        assert_eq!(parse("").unwrap().batch, Batch::Auto);
        assert_eq!(parse("[pointer]\n").unwrap().batch, Batch::Auto);
        assert_eq!(parse("[pointer]\n").unwrap().buffer, DEFAULT_BUFFER);

        for (text, want) in [
            ("\"auto\"", Batch::Auto),
            ("\"adaptive\"", Batch::Adaptive),
            ("\"none\"", Batch::None),
            ("30", Batch::Millis(30)),
            // An explicit zero is "no spacing", i.e. no buffering.
            ("0", Batch::None),
        ] {
            let cfg = parse(&format!("[pointer]\nbatch = {text}\n")).unwrap();
            assert_eq!(cfg.batch, want, "batch = {text}");
        }

        assert_eq!(parse("[pointer]\nbuffer = 4\n").unwrap().buffer, 4);
        assert_eq!(
            parse("[pointer]\naxis_bits = 16\n").unwrap().axis_bits,
            AxisBits::Sixteen
        );
        assert_eq!(
            parse("[pointer]\noverflow = \"carry\"\n").unwrap().overflow,
            Overflow::Carry
        );
        assert_eq!(
            parse("[pointer]\noverflow = \"clamp\"\n").unwrap().overflow,
            Overflow::Clamp
        );

        // Rejections: bad strings, wrong types, out-of-range values.
        assert!(parse("[pointer]\nbatch = \"fast\"\n").is_err());
        assert!(parse("[pointer]\nbatch = true\n").is_err());
        assert!(parse("[pointer]\nbuffer = 0\n").is_err());
        assert!(parse("[pointer]\nbuffer = \"16\"\n").is_err());
        assert!(parse("[pointer]\naxis_bits = 12\n").is_err());
        assert!(parse("[pointer]\naxis_bits = \"8\"\n").is_err());
        assert!(parse("[pointer]\noverflow = \"wrap\"\n").is_err());
        assert!(parse("[pointer]\nunknown = 1\n").is_err());
    }

    #[test]
    fn pairing_and_reconnect_parse() {
        // Absent → accept.
        assert_eq!(parse("").unwrap().pairing, PairingMode::Accept);
        assert_eq!(
            parse("[connection]\n").unwrap().pairing,
            PairingMode::Accept
        );
        assert_eq!(parse("[connection]\n").unwrap().reconnect, None);
        // Explicit pairing values.
        for (text, want) in [
            ("accept", PairingMode::Accept),
            ("prompt_if_possible", PairingMode::PromptIfPossible),
            ("prompt", PairingMode::Prompt),
        ] {
            let cfg = parse(&format!("[connection]\npairing = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.pairing, want, "pairing = {text}");
        }
        assert!(parse("[connection]\npairing = \"maybe\"\n").is_err());
        // The old spellings are gone.
        assert!(parse("[connection]\npairing = \"auto\"\n").is_err());
        assert!(parse("[connection]\npairing = \"confirm\"\n").is_err());
        // Reconnect address.
        assert_eq!(
            parse("[connection]\nreconnect = \"AA:BB:CC:DD:EE:FF\"\n")
                .unwrap()
                .reconnect
                .as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        // Malformed addresses are rejected.
        assert!(parse("[connection]\nreconnect = \"AA:BB:CC:DD:EE\"\n").is_err());
        assert!(parse("[connection]\nreconnect = \"ZZ:BB:CC:DD:EE:FF\"\n").is_err());
        assert!(parse("[connection]\nreconnect = 1\n").is_err());
    }

    #[test]
    fn protocol_parse() {
        // Absent → default (BLE).
        assert_eq!(parse("").unwrap().protocol, Protocol::Ble);
        assert_eq!(parse("[connection]\n").unwrap().protocol, Protocol::Ble);
        // Explicit values.
        assert_eq!(
            parse("[connection]\nprotocol = \"classic\"\n")
                .unwrap()
                .protocol,
            Protocol::Classic
        );
        assert_eq!(
            parse("[connection]\nprotocol = \"ble\"\n")
                .unwrap()
                .protocol,
            Protocol::Ble
        );
        // Bad values are rejected.
        assert!(parse("[connection]\nprotocol = \"le\"\n").is_err());
        assert!(parse("[connection]\nprotocol = 1\n").is_err());
        assert!(parse("[connection]\nunknown = 1\n").is_err());
    }

    #[test]
    fn hotplug_parse() {
        // Absent → default (Auto).
        assert_eq!(parse("").unwrap().hotplug, Hotplug::Auto);
        assert_eq!(parse("[gamepad]\n").unwrap().hotplug, Hotplug::Auto);
        // Explicit values.
        assert_eq!(
            parse("[gamepad]\nhotplug = \"auto\"\n").unwrap().hotplug,
            Hotplug::Auto
        );
        assert_eq!(
            parse("[gamepad]\nhotplug = \"on\"\n").unwrap().hotplug,
            Hotplug::On
        );
        assert_eq!(
            parse("[gamepad]\nhotplug = \"off\"\n").unwrap().hotplug,
            Hotplug::Off
        );
        // Bad values are rejected.
        assert!(parse("[gamepad]\nhotplug = \"foo\"\n").is_err());
        assert!(parse("[gamepad]\nhotplug = 1\n").is_err());
    }

    #[test]
    fn hotplug_enabled() {
        // Auto: on only for a fixed, non-zero count.
        assert!(Hotplug::Auto.enabled(GamepadSlots::Fixed(2)));
        assert!(!Hotplug::Auto.enabled(GamepadSlots::Fixed(0)));
        assert!(!Hotplug::Auto.enabled(GamepadSlots::Initial));
        // On/Off ignore the slot policy.
        assert!(Hotplug::On.enabled(GamepadSlots::Initial));
        assert!(Hotplug::On.enabled(GamepadSlots::Fixed(0)));
        assert!(!Hotplug::Off.enabled(GamepadSlots::Fixed(2)));
    }

    #[test]
    fn gamepad_slots() {
        // Absent → default (Initial).
        assert_eq!(parse("").unwrap().gamepad_slots, GamepadSlots::Initial);
        assert_eq!(
            parse("[gamepad]\n").unwrap().gamepad_slots,
            GamepadSlots::Initial
        );
        // Explicit "initial", a disabling 0, and a fixed count.
        assert_eq!(
            parse("[gamepad]\nslots = \"initial\"\n")
                .unwrap()
                .gamepad_slots,
            GamepadSlots::Initial
        );
        assert_eq!(
            parse("[gamepad]\nslots = 0\n").unwrap().gamepad_slots,
            GamepadSlots::Fixed(0)
        );
        assert_eq!(
            parse("[gamepad]\nslots = 4\n").unwrap().gamepad_slots,
            GamepadSlots::Fixed(4)
        );
        // Bad values are rejected.
        assert!(parse("[gamepad]\nslots = \"foo\"\n").is_err());
        assert!(parse("[gamepad]\nslots = -1\n").is_err());
        assert!(parse("[gamepad]\nunknown = 1\n").is_err());
    }

    #[test]
    fn overrides_and_disable() {
        let hk = parse(
            "[hotkeys]\n\
             exit = \"control+alt+pause\" # either side\n\
             drop_connection = \"\"\n",
        )
        .unwrap()
        .hotkeys;
        // Right Shift is still a trigger (default capture toggle), but bare
        // Right Shift does nothing: drop_connection is disabled.
        assert!(hk.is_trigger(keymap::KEY_RIGHTSHIFT));
        assert_eq!(hk.action(keymap::KEY_RIGHTSHIFT, 0), None);
        assert_eq!(
            hk.action(keymap::KEY_RIGHTSHIFT, 0x02),
            Some(Action::CaptureToggle)
        );
        // Right-side modifiers satisfy the side-agnostic aliases.
        assert_eq!(hk.action(keymap::KEY_PAUSE, 0x50), Some(Action::Exit));
        assert_eq!(hk.action(keymap::KEY_PAUSE, 0x10), None); // alt missing
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("exit = \"pause\"\n").is_err()); // outside table
        assert!(parse("[hotkeys]\nfoo = \"pause\"\n").is_err()); // unknown key
        assert!(parse("[hotkeys]\nexit = pause\n").is_err()); // unquoted
        assert!(parse("[hotkeys]\nexit = \"nosuchkey\"\n").is_err());
        assert!(parse("[hotkeys]\nexit = \"a+scrolllock\"\n").is_err()); // 'a' not a modifier
        assert!(parse("[hotkeys]\nexit = \"pause\"\nexit = \"pause\"\n").is_err()); // duplicate
        assert!(parse("[other]\n").is_err());
    }

    #[test]
    fn trailing_comments_ok() {
        let hk = parse("[hotkeys] # table\nexit = \"pause\" # comment\n")
            .unwrap()
            .hotkeys;
        assert_eq!(hk.action(keymap::KEY_PAUSE, 0), Some(Action::Exit));
    }
}
