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
    /// Tap a Consumer-page button on the virtual remote: one press report and
    /// one release report (design/REMOTE.md §6, §7).
    Consumer(u16),
}

/// The most keys one chord may name (the 8 HID modifiers plus a final key).
/// Bounds the replay buffer in `report`, which is fixed-size.
pub const MAX_CHORD_KEYS: usize = 8;

/// One position in a chord: the keycode(s) that satisfy it. The side-agnostic
/// aliases (`shift`, `ctrl`, …) are satisfied by either side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    One(u16),
    Either(u16, u16),
}

impl Step {
    pub fn matches(self, code: u16) -> bool {
        match self {
            Step::One(a) => code == a,
            Step::Either(a, b) => code == a || code == b,
        }
    }
}

/// A parsed hotkey: the keys that must be pressed for it to fire. `steps[0]` is
/// ordered — it must be pressed first, and pressing it is what starts matching
/// this chord — while `steps[1..]` may be pressed in any order after it
/// (design/ARCH.md §7.3).
#[derive(Clone, Debug)]
pub struct Chord {
    pub steps: Vec<Step>,
    pub action: Action,
}

/// The full hotkey table. Falls back to the built-in right-shift defaults for
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

/// How much of the adapter's identity blooter takes over in BLE mode
/// (design/CONNECTION.md §4.1).
///
/// The LE advertisement's name and appearance only reach a host *before* it
/// connects. Once connected, the host reads the GAP service (0x1800), which
/// bluetoothd owns and serves from the adapter's alias and Class of Device —
/// so without setting those, a host sees the machine's hostname and its
/// computer icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Advertise {
    /// `AliasCod`, but fall back to `Alias` without complaint when the Class of
    /// Device cannot be set (it needs `CAP_NET_ADMIN`) — the default.
    #[default]
    Auto,
    /// Set the adapter alias only. Needs no privilege, but leaves the host
    /// showing the adapter's existing device icon.
    Alias,
    /// Set the alias *and* the Class of Device. Fails at startup if the class
    /// cannot be set.
    AliasCod,
    /// `AliasCod`, plus turning off BR/EDR discoverability so only the LE
    /// identity is on offer.
    AliasCodHide,
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

/// TV-remote emulation: the Consumer Control collection and what feeds it
/// (design/REMOTE.md). The chords bound in `[remote]` are not held here — they
/// are folded into [`Hotkeys`] as `Action::Consumer` chords, so one chord
/// matcher serves the whole config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Advertise the Consumer Control collection. Off by default: turning it on
    /// changes the report descriptor, which already-bonded hosts have cached
    /// (design/REMOTE.md §3.2).
    pub enabled: bool,
    /// Forward the local keyboard's own media keys as consumer usages (§5).
    pub passthrough: bool,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            enabled: false,
            passthrough: true,
        }
    }
}

/// The most `[remote]` bindings one config may declare. Caps `MAX_CHORDS`, and
/// with it the fixed-size candidate array in `report::ChordBuf`; 24 is more
/// buttons than a real remote has (design/REMOTE.md §8).
pub const MAX_REMOTE_BINDINGS: usize = 24;

/// The parsed configuration file.
#[derive(Clone, Debug)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub gamepad_slots: GamepadSlots,
    pub hotplug: Hotplug,
    pub protocol: Protocol,
    /// Pairing agent behaviour (§5).
    pub pairing: PairingMode,
    /// Address of a host to initiate an outgoing HID connection to. Classic
    /// only: BLE is peripheral-only and never dials out (§3.2, §4).
    pub reconnect: Option<String>,
    /// How much of the adapter identity BLE mode takes over (§4.1).
    pub advertise: Advertise,
    /// Pointer batching (§7.2c).
    pub batch: Batch,
    pub buffer: usize,
    pub axis_bits: AxisBits,
    pub overflow: Overflow,
    /// TV-remote emulation (design/REMOTE.md).
    pub remote: Remote,
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
            advertise: Advertise::default(),
            batch: Batch::default(),
            buffer: DEFAULT_BUFFER,
            axis_bits: AxisBits::default(),
            overflow: Overflow::default(),
            remote: Remote::default(),
        }
    }
}

/// The most chords a hotkey table can hold: one per recognized `[hotkeys]` key,
/// plus the `[remote]` bindings (design/REMOTE.md §8).
pub const MAX_CHORDS: usize = DEFAULTS.len() + MAX_REMOTE_BINDINGS;

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
            .map(|(key, spec)| Chord {
                steps: parse_chord_spec(spec).expect("built-in default chords parse"),
                action: action_for(key),
            })
            .collect();
        Hotkeys { chords }
    }
}

impl Hotkeys {
    pub fn chords(&self) -> &[Chord] {
        &self.chords
    }

    /// Whether pressing `code` starts some chord, i.e. whether it opens a chord
    /// buffer instead of being forwarded straight away (design/ARCH.md §7.3).
    pub fn starts(&self, code: u16) -> bool {
        self.chords.iter().any(|c| c.steps[0].matches(code))
    }

    /// Fold in further chords — the `[remote]` bindings, which are matched by
    /// the same machinery as `[hotkeys]` (design/REMOTE.md §6).
    fn extend(&mut self, chords: impl IntoIterator<Item = Chord>) {
        self.chords.extend(chords);
        debug_assert!(self.chords.len() <= MAX_CHORDS, "too many chords");
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
            let ble = top.ble.unwrap_or_default();
            let pointer = top.pointer.unwrap_or_default();
            let remote = top.remote.unwrap_or_default();
            // With the collection off there is nothing for a binding to reach,
            // so the rest of the section is ignored — a warning rather than an
            // error, so a config can be prepared before flipping the switch
            // (design/REMOTE.md §6).
            let mut hotkeys = top.hotkeys.unwrap_or_default();
            if remote.remote.enabled {
                hotkeys.extend(remote.chords);
            } else if !remote.chords.is_empty() {
                log::warn!(
                    "[remote] enabled is false, so its {} binding(s) are ignored",
                    remote.chords.len()
                );
            }
            Ok(Config {
                hotkeys,
                gamepad_slots: gamepad.slots,
                hotplug: gamepad.hotplug,
                protocol: connection.protocol,
                pairing: connection.pairing,
                reconnect: connection.reconnect,
                advertise: ble.advertise,
                batch: pointer.batch,
                buffer: pointer.buffer,
                axis_bits: pointer.axis_bits,
                overflow: pointer.overflow,
                remote: remote.remote,
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

/// The whole config file: the optional `[hotkeys]`, `[gamepad]`, `[connection]`,
/// `[ble]`, `[pointer]` and `[remote]` tables.
struct TopLevel {
    hotkeys: Option<Hotkeys>,
    gamepad: Option<Gamepad>,
    connection: Option<Connection>,
    ble: Option<Ble>,
    pointer: Option<Pointer>,
    remote: Option<RemoteSection>,
}

impl<'de> FromToml<'de> for TopLevel {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let hotkeys = th.optional("hotkeys");
        let gamepad = th.optional("gamepad");
        let connection = th.optional("connection");
        let ble = th.optional("ble");
        let pointer = th.optional("pointer");
        let remote = th.optional("remote");
        th.require_empty()?;
        Ok(TopLevel {
            hotkeys,
            gamepad,
            connection,
            ble,
            pointer,
            remote,
        })
    }
}

/// The `[remote]` table: the two switches, plus every other key in the table
/// read as a binding — a remote-button name or `"usage:0xNNN"` — bound to a
/// chord (design/REMOTE.md §6).
#[derive(Default)]
struct RemoteSection {
    remote: Remote,
    chords: Vec<Chord>,
}

impl<'de> FromToml<'de> for RemoteSection {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let enabled = th.optional_mapped("enabled", bool_item).unwrap_or(false);
        let passthrough = th.optional_mapped("passthrough", bool_item).unwrap_or(true);
        // Everything left is a binding. `require_empty` cannot be used here:
        // the binding names are open-ended, so unknown keys are diagnosed
        // against the binding table below instead.
        let entries: Vec<_> = th.into_remaining().collect();
        let mut chords = Vec::new();
        let mut failed = false;
        for (key, value) in entries {
            let Some(usage) = binding_usage(key.name) else {
                ctx.report_custom_error(
                    format!(
                        "'{}' is not a remote button; use one of the names in \
                         design/REMOTE.md §6, or \"usage:0xNNN\"",
                        key.name
                    ),
                    value,
                );
                failed = true;
                continue;
            };
            match chord_item(value) {
                Ok(Some(steps)) => chords.push(Chord {
                    steps,
                    action: Action::Consumer(usage),
                }),
                Ok(None) => {} // explicitly disabled with ""
                Err(e) => {
                    ctx.push_error(e);
                    failed = true;
                }
            }
        }
        if chords.len() > MAX_REMOTE_BINDINGS {
            ctx.report_custom_error(
                format!("[remote] binds at most {MAX_REMOTE_BINDINGS} buttons"),
                item,
            );
            failed = true;
        }
        if failed {
            return Err(Failed);
        }
        Ok(RemoteSection {
            remote: Remote {
                enabled,
                passthrough,
            },
            chords,
        })
    }
}

/// The Consumer-page usage a `[remote]` key names: a friendly button name, or
/// the `"usage:0xNNN"` escape hatch for anything unlisted — including
/// best-effort usages such as Mode Step, which is deliberately nameless
/// (design/REMOTE.md §2.1, §6).
fn binding_usage(name: &str) -> Option<u16> {
    if let Some(hex) = name.strip_prefix("usage:0x") {
        let usage = u16::from_str_radix(hex, 16).ok()?;
        return (usage <= keymap::MAX_CONSUMER_USAGE).then_some(usage);
    }
    keymap::remote_usage(name)
}

/// Parse a boolean value.
fn bool_item(item: &Item<'_>) -> Result<bool, toml_spanner::Error> {
    item.as_bool()
        .ok_or_else(|| item.expected(&"true or false"))
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

/// The `[ble]` table: settings that only apply to the LE transport.
#[derive(Default)]
struct Ble {
    advertise: Advertise,
}

impl<'de> FromToml<'de> for Ble {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let mut th = item.table_helper(ctx)?;
        let advertise = th
            .optional_mapped("advertise", advertise_item)
            .unwrap_or_default();
        th.require_empty()?;
        Ok(Ble { advertise })
    }
}

/// Parse the `advertise` value: `"auto"`, `"alias"`, `"alias_cod"` or
/// `"alias_cod_hide"`.
fn advertise_item(item: &Item<'_>) -> Result<Advertise, toml_spanner::Error> {
    match item.as_str() {
        Some("auto") => Ok(Advertise::Auto),
        Some("alias") => Ok(Advertise::Alias),
        Some("alias_cod") => Ok(Advertise::AliasCod),
        Some("alias_cod_hide") => Ok(Advertise::AliasCodHide),
        _ => Err(item.expected(&"\"auto\", \"alias\", \"alias_cod\" or \"alias_cod_hide\"")),
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
                Some(Some(steps)) => chords.push(Chord {
                    steps,
                    action: action_for(key),
                }),
                Some(None) => {} // explicitly disabled with ""
                None => {
                    // Absent (or invalid, in which case the error is already
                    // recorded and the parse fails): fall back to the default.
                    if !default.is_empty() {
                        chords.push(Chord {
                            steps: parse_chord_spec(default)
                                .expect("built-in default chords parse"),
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
fn chord_item(item: &Item<'_>) -> Result<Option<Vec<Step>>, toml_spanner::Error> {
    let spec = item.as_str().ok_or_else(|| item.expected(&"a string"))?;
    if spec.is_empty() {
        return Ok(None);
    }
    parse_chord_spec(spec)
        .map(Some)
        .map_err(|e| toml_spanner::Error::custom_at(e, item))
}

/// Parse a chord spec: zero or more modifier names and a final trigger key,
/// joined with '+'. Key names follow keyd's keycode table. Returns one `Step`
/// per named key, in the order written.
fn parse_chord_spec(spec: &str) -> Result<Vec<Step>, String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    if parts.len() > MAX_CHORD_KEYS {
        return Err(format!("a chord names at most {MAX_CHORD_KEYS} keys"));
    }
    let (&trigger_name, mod_names) = parts.split_last().expect("split yields at least one part");
    let mut steps: Vec<Step> = mod_names
        .iter()
        .map(|name| modifier_step(name).ok_or_else(|| format!("'{name}' is not a modifier key")))
        .collect::<Result<_, _>>()?;
    let trigger = keymap::keycode_from_name(trigger_name)
        .ok_or_else(|| format!("unknown key name '{trigger_name}'"))?;
    steps.push(Step::One(trigger));
    Ok(steps)
}

/// The step a chord-modifier name stands for. Side-specific names match that
/// side only; the side-agnostic aliases match whichever side is pressed.
fn modifier_step(name: &str) -> Option<Step> {
    use keymap::*;
    Some(match name {
        "control" | "ctrl" => Step::Either(KEY_LEFTCTRL, KEY_RIGHTCTRL),
        "shift" => Step::Either(KEY_LEFTSHIFT, KEY_RIGHTSHIFT),
        "alt" => Step::Either(KEY_LEFTALT, KEY_RIGHTALT),
        "meta" | "super" => Step::Either(KEY_LEFTMETA, KEY_RIGHTMETA),
        _ => {
            let code = keymap::keycode_from_name(name)?;
            keymap::modifier_bit(code)?;
            Step::One(code)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap;

    /// The steps of the chord bound to `action`, if it is enabled.
    fn steps(hk: &Hotkeys, action: Action) -> Option<&[Step]> {
        hk.chords()
            .iter()
            .find(|c| c.action == action)
            .map(|c| c.steps.as_slice())
    }

    #[test]
    fn defaults() {
        let hk = Hotkeys::default();
        // Right Shift completes both default chords but starts neither, so a
        // bare Right Shift is forwarded like any other key.
        assert!(!hk.starts(keymap::KEY_RIGHTSHIFT));
        assert!(!hk.starts(keymap::KEY_A));
        assert!(hk.starts(keymap::KEY_LEFTCTRL));
        assert!(hk.starts(keymap::KEY_LEFTSHIFT));
        // drop_connection is disabled by default.
        assert_eq!(steps(&hk, Action::DropConnection), None);
        assert_eq!(
            steps(&hk, Action::Exit),
            Some(
                [
                    Step::One(keymap::KEY_LEFTCTRL),
                    Step::One(keymap::KEY_LEFTALT),
                    Step::One(keymap::KEY_RIGHTSHIFT),
                ]
                .as_slice()
            )
        );
        assert_eq!(
            steps(&hk, Action::CaptureToggle),
            Some(
                [
                    Step::One(keymap::KEY_LEFTSHIFT),
                    Step::One(keymap::KEY_RIGHTSHIFT),
                ]
                .as_slice()
            )
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
        assert_eq!(cfg.remote, Remote::default());
    }

    /// The usages the named `[remote]` bindings resolve to, in config order.
    fn consumer_usages(hk: &Hotkeys) -> Vec<u16> {
        hk.chords()
            .iter()
            .filter_map(|c| match c.action {
                Action::Consumer(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn remote_parse() {
        // Absent → off, and off is the state in which the descriptor is
        // unchanged (design/REMOTE.md §3.1).
        assert_eq!(parse("").unwrap().remote, Remote::default());
        assert!(!parse("[remote]\n").unwrap().remote.enabled);
        // Passthrough defaults on once enabled, and can be turned off.
        let cfg = parse("[remote]\nenabled = true\n").unwrap();
        assert_eq!(
            cfg.remote,
            Remote {
                enabled: true,
                passthrough: true
            }
        );
        assert!(
            !parse("[remote]\nenabled = true\npassthrough = false\n")
                .unwrap()
                .remote
                .passthrough
        );

        // Named bindings become Consumer chords alongside the hotkey defaults.
        let cfg = parse(
            "[remote]\n\
             enabled = true\n\
             tv = \"leftmeta+t\"\n\
             channel_up = \"leftmeta+pageup\"\n\
             all_apps = \"\"\n",
        )
        .unwrap();
        assert_eq!(consumer_usages(&cfg.hotkeys), [0x089, 0x09C]);
        // They match through the same machinery as `[hotkeys]`, so a bare meta
        // now opens a chord buffer.
        assert!(cfg.hotkeys.starts(keymap::KEY_LEFTMETA));
        // The defaults survive.
        assert!(steps(&cfg.hotkeys, Action::CaptureToggle).is_some());

        // The escape hatch takes any usage the descriptor declares — including
        // Mode Step, which has no friendly name on purpose (§2.1).
        let cfg = parse("[remote]\nenabled = true\n\"usage:0x082\" = \"leftmeta+s\"\n").unwrap();
        assert_eq!(consumer_usages(&cfg.hotkeys), [0x082]);
        assert_eq!(
            consumer_usages(
                &parse("[remote]\nenabled = true\n\"usage:0x2A2\" = \"leftmeta+a\"\n")
                    .unwrap()
                    .hotkeys
            ),
            [0x2A2]
        );

        // Disabled: the bindings are ignored rather than rejected, so a config
        // can be prepared before flipping the switch (§6).
        let cfg = parse("[remote]\ntv = \"leftmeta+t\"\n").unwrap();
        assert_eq!(consumer_usages(&cfg.hotkeys), [] as [u16; 0]);
        assert!(!cfg.hotkeys.starts(keymap::KEY_LEFTMETA));
    }

    #[test]
    fn remote_rejects_bad_input() {
        // Unknown button names, and the ones §2.1 deliberately refuses to offer.
        assert!(parse("[remote]\nenabled = true\nsource = \"leftmeta+s\"\n").is_err());
        assert!(parse("[remote]\nenabled = true\ninput = \"leftmeta+i\"\n").is_err());
        // Malformed or out-of-range escape hatches.
        assert!(parse("[remote]\nenabled = true\n\"usage:0x2A3\" = \"leftmeta+s\"\n").is_err());
        assert!(parse("[remote]\nenabled = true\n\"usage:0xZZ\" = \"leftmeta+s\"\n").is_err());
        assert!(parse("[remote]\nenabled = true\n\"usage:130\" = \"leftmeta+s\"\n").is_err());
        // Bad chords and wrong value types.
        assert!(parse("[remote]\nenabled = true\ntv = \"leftmeta+nosuchkey\"\n").is_err());
        assert!(parse("[remote]\nenabled = true\ntv = 1\n").is_err());
        assert!(parse("[remote]\nenabled = \"yes\"\n").is_err());
        // Bindings are checked even while disabled; only their *effect* is
        // dropped, so a typo is still reported.
        assert!(parse("[remote]\nnosuchbutton = \"leftmeta+s\"\n").is_err());
        // More bindings than the fixed chord buffer can hold.
        let many: String = (0..=MAX_REMOTE_BINDINGS)
            .map(|i| format!("\"usage:0x{i:03x}\" = \"leftmeta+a\"\n"))
            .collect();
        assert!(parse(&format!("[remote]\nenabled = true\n{many}")).is_err());
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
    fn advertise_parses() {
        // Absent → auto, whether or not the table is present.
        assert_eq!(parse("").unwrap().advertise, Advertise::Auto);
        assert_eq!(parse("[ble]\n").unwrap().advertise, Advertise::Auto);
        for (text, want) in [
            ("auto", Advertise::Auto),
            ("alias", Advertise::Alias),
            ("alias_cod", Advertise::AliasCod),
            ("alias_cod_hide", Advertise::AliasCodHide),
        ] {
            let cfg = parse(&format!("[ble]\nadvertise = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.advertise, want, "advertise = {text}");
        }
        // Values are snake_case, matching `pairing`; kebab-case is not accepted.
        assert!(parse("[ble]\nadvertise = \"alias-cod\"\n").is_err());
        assert!(parse("[ble]\nadvertise = \"none\"\n").is_err());
        assert!(parse("[ble]\nadvertise = true\n").is_err());
        assert!(parse("[ble]\nunknown = 1\n").is_err());
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
        // The default capture toggle survives; drop_connection is disabled.
        assert_eq!(steps(&hk, Action::DropConnection), None);
        assert_eq!(
            steps(&hk, Action::CaptureToggle),
            Some(
                [
                    Step::One(keymap::KEY_LEFTSHIFT),
                    Step::One(keymap::KEY_RIGHTSHIFT),
                ]
                .as_slice()
            )
        );
        // The side-agnostic aliases accept either side.
        let exit = steps(&hk, Action::Exit).unwrap();
        assert!(exit[0].matches(keymap::KEY_RIGHTCTRL));
        assert!(exit[0].matches(keymap::KEY_LEFTCTRL));
        assert!(exit[1].matches(keymap::KEY_RIGHTALT));
        assert_eq!(exit[2], Step::One(keymap::KEY_PAUSE));
        // Either-side aliases start a chord from either side.
        assert!(hk.starts(keymap::KEY_RIGHTCTRL));
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
        // Longer than the fixed-size chord buffer can hold.
        let long = ["leftshift"; MAX_CHORD_KEYS + 1].join("+");
        assert!(parse(&format!("[hotkeys]\nexit = \"{long}\"\n")).is_err());
    }

    #[test]
    fn trailing_comments_ok() {
        let hk = parse("[hotkeys] # table\nexit = \"pause\" # comment\n")
            .unwrap()
            .hotkeys;
        assert_eq!(
            steps(&hk, Action::Exit),
            Some([Step::One(keymap::KEY_PAUSE)].as_slice())
        );
    }
}
