//! Interactive host-(re)connection menu (design/CONNECTION.md §6).
//!
//! A small, pre-emptable TUI built directly on `crossterm`'s async
//! [`EventStream`]: arrow keys move, number keys pick a host, letter keys drive
//! actions ("Other devices" submenu, rescan, skip). It runs as a spawned task
//! that the active transport races against an incoming connection; a `oneshot`
//! cancel signal (fired on inbound-accept or shutdown) preempts the menu at any
//! await point and the terminal is always restored.
//!
//! Both transports use it, but for **different jobs** — [`Kind`] is not a mere
//! discovery filter (design/CONNECTION.md §6):
//!
//! - [`Kind::Classic`] is a *host picker*. It scans for BR/EDR devices, pairs a
//!   newly-picked one from here, and hands back an address to dial. Ineligible
//!   devices — Bluetooth audio/headsets and other HID peripherals, and devices
//!   with no real name (only a hex identifier) — are moved to an "Other devices"
//!   submenu so the main list shows just plausible HID hosts
//!   (computers/phones/TVs). The test is a deny-list, so an unrecognised device
//!   stays in the main list ([`is_other`]), and already-paired devices always do:
//!   bonding one was a deliberate choice.
//! - [`Kind::Ble`] is a *bonded-host manager*, and deliberately does none of
//!   that. blooter is a BLE peripheral: hosts are centrals, they do not
//!   advertise, so scanning cannot find them and there is nothing to dial or to
//!   pair from this side. It lists the hosts blooter is bonded to and offers the
//!   two things that *are* ours to do — `[f]` fix a stale layout and `[u]` drop
//!   the bond — while the host does the connecting and the pairing.
//!
//! [`Session`] wraps the spawn/cancel/join plumbing so each transport's
//! `wait_connected` drives the menu the same way.

use std::future;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport};
use crossterm::cursor;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use futures::StreamExt;
use log::{info, trace, warn};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// How long to scan on entry and on each rescan.
const SCAN_SECS: u64 = 4;

/// Which transport the menu is running for. The list and the key handling are
/// otherwise identical (design/CONNECTION.md §6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Classic,
    Ble,
}

impl Kind {
    /// Scan only on the transport in use, so the list never offers a device the
    /// caller cannot connect to. BLE does not scan at all.
    fn discovery_transport(self) -> DiscoveryTransport {
        match self {
            Self::Classic => DiscoveryTransport::BrEdr,
            Self::Ble => DiscoveryTransport::Le,
        }
    }

    /// Whether the menu picks a host to connect out to. Only Classic does:
    /// initiating is not something a BLE peripheral can do (§4).
    fn picks_hosts(self) -> bool {
        self == Self::Classic
    }
}

// --- Pure model ----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Screen {
    Main,
    Other,
}

/// A display row for a device. Holds no [`Device`] handle, so the rendering and
/// key-handling logic is pure and unit-testable; the live handles live in the
/// parallel `*_devs` vectors of [`MenuState`].
struct Row {
    addr: Address,
    alias: String,
    connected: bool,
    paired: bool,
    /// Signal strength, from a discovery scan. Always `None` on BLE, which does
    /// not scan.
    rssi: Option<i16>,
    /// Bonded under a different HID report descriptor than the current one, so
    /// this host is still using a cached copy that no longer matches what
    /// blooter sends (design/CONNECTION.md §7). Fixable with `[f]`.
    stale: bool,
    /// blooter remembers bonding this host, but bluetoothd has no device object
    /// for it any more. Kept in the list rather than dropped, so a host cannot
    /// silently vanish from the only UI that can act on it.
    forgotten_by_bluez: bool,
}

/// What the menu resolved to (design/CONNECTION.md §6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// Start a session on this host: pair it if new, then dial it. Classic only
    /// — a BLE peripheral cannot initiate (§4).
    Connect(Address),
    /// Repair this host rather than starting a session on it: make it drop the
    /// copy of blooter's HID layout it cached when it bonded, by whatever means
    /// the transport has (§7).
    Fix(Address),
    /// Drop blooter's bond to this host. Always an explicit user choice — never
    /// something a failed operation does on its own (§7.2b).
    Forget(Address),
}

/// What a keypress asks the event loop to do. Cursor moves and screen switches
/// are applied to the state in place and reported as `None` (the loop redraws
/// every iteration regardless).
#[derive(PartialEq, Eq, Debug)]
enum Action {
    None,
    /// Select the device at this index within the active screen's list.
    Select(usize),
    /// Fix the connection to the device at this index (§7).
    Fix(usize),
    /// Drop the bond to the device at this index (§7.2b).
    Forget(usize),
    Rescan,
    Skip,
}

struct MenuState {
    kind: Kind,
    screen: Screen,
    main: Vec<Row>,
    other: Vec<Row>,
    main_devs: Vec<Device>,
    other_devs: Vec<Device>,
    selected: usize,
}

impl MenuState {
    fn rows(&self) -> &[Row] {
        match self.screen {
            Screen::Main => &self.main,
            Screen::Other => &self.other,
        }
    }

    fn devs(&self) -> &[Device] {
        match self.screen {
            Screen::Main => &self.main_devs,
            Screen::Other => &self.other_devs,
        }
    }
}

/// GAP Appearance categories (`appearance >> 6`) that cannot be a HID host: a
/// HID peripheral like another keyboard or mouse, and LE audio devices.
const APPEARANCE_HID: u16 = 0x0f;
const APPEARANCE_AUDIO_SINK: u16 = 0x21;
const APPEARANCE_AUDIO_SOURCE: u16 = 0x22;

/// Minor device classes within the Audio/Video major class that are genuinely
/// speakers, microphones and cameras. Everything *else* under that major class —
/// uncategorised (0x00), car audio (0x08), set-top boxes, monitors, video
/// conferencing, consoles — is a plausible HID host: TVs and streaming dongles
/// share the Audio/Video major class with headsets and are wildly inconsistent
/// about which minor class they claim (a Google TV in the wild reports 0x08).
const AV_MINOR_AUDIO: [u32; 9] = [
    0x01, // Wearable Headset
    0x02, // Hands-free
    0x04, // Microphone
    0x05, // Loudspeaker
    0x06, // Headphones
    0x07, // Portable Audio
    0x0a, // HiFi Audio
    0x0c, // Video Camera
    0x0d, // Camcorder
];

/// True if a device belongs in the "Other devices" submenu rather than the main
/// host list.
///
/// This is a *deny-list*: a device is demoted only on positive evidence that it
/// is a peripheral, and anything unrecognised stays in the main list. The bias is
/// deliberate — a stray car stereo in the host list costs one line, while hiding
/// a real TV costs the user the feature. An earlier allow-list of known-good
/// classes did exactly that, since real hosts advertise classes nobody guesses in
/// advance.
///
/// A bond outranks every heuristic: pairing is an explicit decision that this
/// device is a host worth using, so a paired device is never filed under "Other"
/// whatever it claims to be. Otherwise a device is "Other" if it has no real
/// name, if its Class of Device marks it as a peripheral, or if its GAP
/// Appearance marks it as another HID peripheral or an audio device. Both
/// property checks are applied whenever the property is present — LE-only peers
/// have no Class of Device and BR/EDR peers usually have no Appearance, so each
/// simply falls through for the other.
///
/// Note that the Audio *service* class bit (bit 21) is deliberately not consulted
/// and must not be reinstated: laptops, phones and TVs all advertise A2DP, so it
/// distinguishes nothing.
fn is_other(
    class: Option<u32>,
    appearance: Option<u16>,
    has_real_name: bool,
    paired: bool,
) -> bool {
    if paired {
        return false;
    }
    if !has_real_name {
        return true;
    }
    if let Some(c) = class {
        // Major device class is bits 8-12, minor device class bits 2-7.
        let major = (c >> 8) & 0x1f;
        let minor = (c >> 2) & 0x3f;
        let peripheral = match major {
            4 => AV_MINOR_AUDIO.contains(&minor), // headset/speaker/mic/camera
            5 => true,                            // Peripheral: keyboard, mouse, joystick
            6 => true,                            // Imaging: printer, scanner, camera
            7 => true,                            // Wearable
            8 => true,                            // Toy
            9 => true,                            // Health
            // Miscellaneous (0), Computer (1), Phone (2), LAN (3), the non-audio
            // half of Audio/Video (4) and Uncategorized (31) are all plausible
            // hosts, as is any major class not yet assigned.
            _ => false,
        };
        if peripheral {
            return true;
        }
    }
    matches!(
        appearance.map(|a| a >> 6),
        Some(APPEARANCE_HID | APPEARANCE_AUDIO_SINK | APPEARANCE_AUDIO_SOURCE)
    )
}

/// Map a keypress to an [`Action`], applying cursor/screen changes in place.
fn on_key(state: &mut MenuState, key: KeyEvent) -> Action {
    let len = state.rows().len();
    match key.code {
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
            Action::None
        }
        KeyCode::Down => {
            if state.selected + 1 < len {
                state.selected += 1;
            }
            Action::None
        }
        // Number keys pick a host to connect to where that means something; on
        // BLE there is nothing to connect to, so they only move the cursor.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            match (idx < len, state.kind.picks_hosts()) {
                (false, _) => Action::None,
                (true, true) => Action::Select(idx),
                (true, false) => {
                    state.selected = idx;
                    Action::None
                }
            }
        }
        KeyCode::Enter => {
            if len > 0 && state.kind.picks_hosts() {
                Action::Select(state.selected)
            } else {
                Action::Skip
            }
        }
        KeyCode::Char('o') | KeyCode::Char('O') => {
            if state.screen == Screen::Main && !state.other.is_empty() {
                state.screen = Screen::Other;
                state.selected = 0;
            }
            Action::None
        }
        KeyCode::Char('b') | KeyCode::Char('B') | KeyCode::Left => {
            if state.screen == Screen::Other {
                state.screen = Screen::Main;
                state.selected = 0;
            }
            Action::None
        }
        // Only bonded hosts have a cached record to invalidate; on anything else
        // the key is a no-op.
        KeyCode::Char('f') | KeyCode::Char('F') => match state.rows().get(state.selected) {
            Some(r) if r.paired => Action::Fix(state.selected),
            _ => Action::None,
        },
        // Dropping a bond is destructive and irreversible from here — the host
        // has to pair again — so it is offered only where it is the documented
        // repair, on BLE, and only on a host actually bonded (§7.2b).
        KeyCode::Char('u') | KeyCode::Char('U') => match state.rows().get(state.selected) {
            Some(r) if r.paired && !state.kind.picks_hosts() => Action::Forget(state.selected),
            _ => Action::None,
        },
        KeyCode::Char('r') | KeyCode::Char('R') => Action::Rescan,
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Skip,
        KeyCode::Esc => {
            if state.screen == Screen::Other {
                state.screen = Screen::Main;
                state.selected = 0;
                Action::None
            } else {
                Action::Skip
            }
        }
        _ => Action::None,
    }
}

/// Render the current screen to a list of lines (pure; the caller does the I/O).
fn render_lines(state: &MenuState) -> Vec<String> {
    let mut lines = Vec::new();
    let ble = !state.kind.picks_hosts();
    let (title, rows) = match (state.screen, ble) {
        // On BLE the list is not something to choose from, so the title says
        // what the user is actually meant to do: pair from the host.
        (_, true) => (
            "Paired hosts (pair new ones from the host's Bluetooth settings):",
            &state.main,
        ),
        (Screen::Main, false) => ("Bluetooth hosts:", &state.main),
        (Screen::Other, false) => ("Other devices:", &state.other),
    };
    lines.push(title.to_string());
    if rows.is_empty() {
        lines.push(if ble {
            "  (none yet)".to_string()
        } else {
            "  (none found)".to_string()
        });
    }
    for (i, r) in rows.iter().enumerate() {
        let marker = if i == state.selected { '>' } else { ' ' };
        let st = if r.connected {
            "connected"
        } else if r.paired {
            "paired"
        } else {
            "unpaired"
        };
        let sig = r.rssi.map(|v| format!(", {v} dBm")).unwrap_or_default();
        let stale = if r.stale { ", stale" } else { "" };
        let gone = if r.forgotten_by_bluez {
            ", unknown to bluetoothd"
        } else {
            ""
        };
        lines.push(format!(
            "{marker} {}. {}  {} [{st}{sig}{stale}{gone}]",
            i + 1,
            r.addr,
            r.alias
        ));
    }
    lines.push(String::new());
    // `[f]` applies to bonded hosts only, so it is offered only when the cursor
    // is on one; `[u]` likewise, and only on BLE.
    let selected = rows.get(state.selected);
    let bonded = matches!(selected, Some(r) if r.paired);
    let fix = if bonded { "[f] Fix connection   " } else { "" };
    let footer = match (state.screen, ble) {
        (_, true) => {
            let forget = if bonded { "[u] Forget host   " } else { "" };
            format!("{fix}{forget}[r] Refresh   [q] Close")
        }
        (Screen::Main, false) if state.other.is_empty() => format!("{fix}[r] Rescan   [q] Skip"),
        (Screen::Main, false) => format!(
            "[o] Other devices ({})   {fix}[r] Rescan   [q] Skip",
            state.other.len()
        ),
        (Screen::Other, false) => format!("[b] Back   {fix}[r] Rescan   [q] Skip"),
    };
    lines.push(footer);
    if rows.iter().any(|r| r.stale) {
        // The two repairs differ: Classic tears the bond down and needs a fresh
        // pairing, BLE tells the host to re-read over the existing one.
        lines.push(if ble {
            "A host marked 'stale' cached an older HID descriptor and will not see \
             blooter's current one; connect from it, then press [f]."
                .to_string()
        } else {
            "A host marked 'stale' cached an older HID descriptor and will not see \
             blooter's current one; [f] fixes it (re-pair afterwards)."
                .to_string()
        });
    }
    lines
}

// --- Terminal ownership coordination -------------------------------------

/// A request to the running menu to release the controlling terminal so the
/// pairing agent can prompt on the TTY. The menu restores cooked mode, sends
/// `ack`, then blocks on `resume` before taking the terminal back
/// (design/CONNECTION.md §5/§6).
struct SuspendReq {
    ack: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

/// Shared coordinator for exclusive use of the controlling terminal between the
/// interactive menu (which holds raw mode plus a crossterm [`EventStream`]) and
/// the pairing agent's TTY prompt.
///
/// An incoming pairing confirmation arrives *while the menu is up*: BlueZ fires
/// the agent's `request_confirmation` before our L2CAP accept completes. Without
/// coordination the menu's `EventStream` reads and discards the "y"/"n"
/// keystrokes meant for the prompt's line read, so pairing is never confirmed
/// and the connection never establishes. The agent borrows the terminal via
/// [`TermCoord::borrow`], which suspends the current menu (if any) — dropping it
/// out of raw mode and pausing its input reads — and resumes it when the
/// returned guard drops. Cloneable and cheap: one clone lives in the agent,
/// another is handed to each spawned menu.
#[derive(Clone, Default)]
pub struct TermCoord {
    inner: Arc<Mutex<Option<mpsc::Sender<SuspendReq>>>>,
}

impl TermCoord {
    /// Register the running menu's suspend channel, returning the receiver the
    /// menu loop selects on. Replaces any previous registration.
    fn register(&self) -> mpsc::Receiver<SuspendReq> {
        let (tx, rx) = mpsc::channel(1);
        *self.inner.lock().unwrap() = Some(tx);
        rx
    }

    /// Clear the registration when the menu ends, so a later borrow is a no-op
    /// rather than signalling a dead menu task.
    fn deregister(&self) {
        *self.inner.lock().unwrap() = None;
    }

    /// Borrow the terminal for a TTY prompt. Suspends the running menu (restoring
    /// cooked mode) and waits for it to acknowledge before returning; the menu
    /// resumes when the returned [`TermBorrow`] is dropped. When no menu is
    /// running (non-interactive, LE, or the menu already resolved) this is a
    /// no-op and the caller simply owns the (already cooked) terminal.
    pub async fn borrow(&self) -> TermBorrow {
        // Clone the sender out under the lock, then release it before awaiting.
        let sender = self.inner.lock().unwrap().clone();
        if let Some(sender) = sender {
            let (ack_tx, ack_rx) = oneshot::channel();
            let (resume_tx, resume_rx) = oneshot::channel();
            let req = SuspendReq {
                ack: ack_tx,
                resume: resume_rx,
            };
            // If the menu is gone between the clone and now, fall through to the
            // no-op guard.
            if sender.send(req).await.is_ok() && ack_rx.await.is_ok() {
                return TermBorrow {
                    resume: Some(resume_tx),
                };
            }
        }
        TermBorrow { resume: None }
    }
}

/// Guard returned by [`TermCoord::borrow`]; resumes the suspended menu on drop
/// (a no-op if no menu was suspended).
pub struct TermBorrow {
    resume: Option<oneshot::Sender<()>>,
}

impl Drop for TermBorrow {
    fn drop(&mut self) {
        if let Some(tx) = self.resume.take() {
            let _ = tx.send(());
        }
    }
}

// --- Terminal handling ---------------------------------------------------

/// Restores the terminal (cooked mode, cursor shown) on drop. Because the
/// release profile uses `panic = "abort"`, `Drop` does not run on a panic, so
/// [`install_panic_hook`] provides the same restore on that path.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
    }
}

/// Ensure a raw-mode terminal is restored even if some future code panics
/// (which, under `panic = "abort"`, would otherwise skip `TermGuard::drop`).
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), cursor::Show);
            prev(info);
        }));
    });
}

/// Clear the previously drawn block (if any) and print `lines`. Returns the new
/// line count to pass back as `prev` next time. Lines end with `\r\n` because
/// the terminal is in raw mode while the menu is up.
fn draw_lines(out: &mut impl Write, lines: &[String], prev: usize) -> io::Result<usize> {
    if prev > 0 {
        queue!(
            out,
            cursor::MoveToPreviousLine(prev as u16),
            Clear(ClearType::FromCursorDown)
        )?;
    } else {
        queue!(
            out,
            cursor::MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )?;
    }
    for line in lines {
        queue!(out, Print(line.as_str()), Print("\r\n"))?;
    }
    out.flush()?;
    Ok(lines.len())
}

fn scanning_line() -> Vec<String> {
    vec!["Scanning for Bluetooth devices…".to_string()]
}

// --- Discovery -----------------------------------------------------------

/// Read each known device's properties and partition into (main hosts, other
/// devices) per [`is_other`]. Mirrors the per-device reads of the previous
/// `setup::host_menu`, plus `class()`/`name()` for classification.
async fn collect(adapter: &Adapter, stale: &[Address]) -> (Vec<(Row, Device)>, Vec<(Row, Device)>) {
    let mut main = Vec::new();
    let mut other = Vec::new();
    for addr in adapter.device_addresses().await.unwrap_or_default() {
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        let class = dev.class().await.ok().flatten();
        let appearance = dev.appearance().await.ok().flatten();
        // `alias()` always yields a name (falling back to the MAC string), so the
        // "no real name" test is on `name()`, which is None when unset.
        let has_real_name = dev.name().await.ok().flatten().is_some();
        let paired = dev.is_paired().await.unwrap_or(false);
        let row = Row {
            addr,
            alias: dev.alias().await.unwrap_or_else(|_| addr.to_string()),
            connected: dev.is_connected().await.unwrap_or(false),
            paired,
            rssi: dev.rssi().await.ok().flatten(),
            // Only a bond carries a cached record, so only bonded hosts can be stale.
            stale: paired && stale.contains(&addr),
            forgotten_by_bluez: false,
        };
        let other_dev = is_other(class, appearance, has_real_name, paired);
        // Everything the classification saw, so a misfiled host can be diagnosed
        // from a log rather than by replaying BlueZ properties by hand. Below
        // `-d`, at `RUST_LOG=trace`: this prints mid-menu and scribbles over the
        // TUI, so it must not appear at a level a user might leave switched on.
        trace!(
            "{addr} {alias:?}: class={} appearance={} named={has_real_name} paired={paired} → {}",
            class.map_or("none".to_string(), |c| format!("{c:#08x}")),
            appearance.map_or("none".to_string(), |a| format!("{a:#06x}")),
            if other_dev { "other" } else { "main" },
            alias = row.alias,
        );
        if other_dev {
            other.push((row, dev));
        } else {
            main.push((row, dev));
        }
    }
    (main, other)
}

/// The BLE list: the hosts blooter is bonded to, with no discovery at all.
///
/// Scanning would be pointless — a host is a GAP Central and does not advertise
/// — so the rows come from what is already known: bluetoothd's bonded devices,
/// unioned with every address blooter has a recorded fingerprint for. That union
/// is the point. `RemoveDevice` deletes the D-Bus object outright, and a host
/// that is not advertising can never be rediscovered, so a list built from
/// bluetoothd alone can lose a host permanently (design/CONNECTION.md §6).
async fn collect_bonded(adapter: &Adapter, stale: &[Address], known: &[Address]) -> Vec<Row> {
    let live = adapter.device_addresses().await.unwrap_or_default();
    let mut rows = Vec::new();
    for &addr in &live {
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        if !dev.is_paired().await.unwrap_or(false) {
            continue;
        }
        rows.push(Row {
            addr,
            alias: dev.alias().await.unwrap_or_else(|_| addr.to_string()),
            connected: dev.is_connected().await.unwrap_or(false),
            paired: true,
            rssi: None,
            stale: stale.contains(&addr),
            forgotten_by_bluez: false,
        });
    }
    // Hosts blooter remembers but bluetoothd no longer has an object for. Shown
    // so `[u]` can clear the leftover record rather than leaving it invisible.
    for &addr in known {
        if live.contains(&addr) {
            continue;
        }
        rows.push(Row {
            addr,
            alias: addr.to_string(),
            connected: false,
            paired: true,
            rssi: None,
            stale: stale.contains(&addr),
            forgotten_by_bluez: true,
        });
    }
    // Connected first, then the rest by address so the order is stable between
    // refreshes (there is no RSSI to rank by).
    rows.sort_by_key(|r| (!r.connected, r.forgotten_by_bluez, r.addr.to_string()));
    rows
}

/// Connected first, then paired, then unpaired; each group strongest signal
/// first (matching the previous menu's ordering).
fn sort_entries(v: &mut [(Row, Device)]) {
    v.sort_by_key(|(r, _)| {
        let group = if r.connected {
            0
        } else if r.paired {
            1
        } else {
            2
        };
        (group, std::cmp::Reverse(r.rssi.unwrap_or(i16::MIN)))
    });
}

/// Build a fresh [`MenuState`]: a discovery pass then a partition on Classic,
/// a plain re-read of the bonded hosts on BLE. Returns `None` only if cancelled
/// mid-scan.
async fn scan(
    adapter: &Adapter,
    kind: Kind,
    stale: &[Address],
    known: &[Address],
    cancel: &mut oneshot::Receiver<()>,
) -> Option<MenuState> {
    if !kind.picks_hosts() {
        return Some(MenuState {
            kind,
            screen: Screen::Main,
            main: collect_bonded(adapter, stale, known).await,
            other: Vec::new(),
            main_devs: Vec::new(),
            other_devs: Vec::new(),
            selected: 0,
        });
    }
    // Restrict the scan to the transport in use. Best-effort: a controller that
    // rejects the filter still gets the default (interleaved) scan.
    let filter = DiscoveryFilter {
        transport: kind.discovery_transport(),
        ..Default::default()
    };
    if let Err(e) = adapter.set_discovery_filter(filter).await {
        warn!("could not restrict discovery to {kind:?}: {e}");
    }
    match adapter.discover_devices().await {
        Ok(mut events) => {
            let end = Instant::now() + Duration::from_secs(SCAN_SECS);
            loop {
                tokio::select! {
                    _ = &mut *cancel => return None,
                    _ = sleep_until(end) => break,
                    ev = events.next() => if ev.is_none() { break },
                }
            }
        }
        Err(e) => warn!("device discovery failed: {e}"),
    }
    let (mut main, mut other) = collect(adapter, stale).await;
    sort_entries(&mut main);
    sort_entries(&mut other);
    let (main, main_devs): (Vec<Row>, Vec<Device>) = main.into_iter().unzip();
    let (other, other_devs): (Vec<Row>, Vec<Device>) = other.into_iter().unzip();
    Some(MenuState {
        kind,
        screen: Screen::Main,
        main,
        other,
        main_devs,
        other_devs,
        selected: 0,
    })
}

/// One cancellable pairing attempt. `None` means the menu was cancelled.
async fn pair_once(dev: &Device, cancel: &mut oneshot::Receiver<()>) -> Option<bluer::Result<()>> {
    tokio::select! {
        _ = &mut *cancel => None,
        r = dev.pair() => Some(r),
    }
}

/// Re-run a short discovery so bluetoothd recreates a device object it dropped,
/// returning early once `addr` reappears. `None` means the menu was cancelled.
async fn rediscover(
    adapter: &Adapter,
    addr: Address,
    cancel: &mut oneshot::Receiver<()>,
) -> Option<()> {
    let Ok(mut events) = adapter.discover_devices().await else {
        return Some(());
    };
    let end = Instant::now() + Duration::from_secs(SCAN_SECS);
    loop {
        tokio::select! {
            _ = &mut *cancel => return None,
            _ = sleep_until(end) => return Some(()),
            ev = events.next() => match ev {
                Some(AdapterEvent::DeviceAdded(a)) if a == addr => return Some(()),
                None => return Some(()),
                _ => {}
            },
        }
    }
}

/// Pair a newly-picked (unbonded) host from here — a deliberate, single-initiator
/// action (design/CONNECTION.md §5). Runs in cooked mode (the raw-mode guard is
/// dropped before this) but stays cancellable so an incoming connection during
/// pairing still preempts. Returns the address to initiate an outgoing HID
/// connection to, or `None` on cancel/failure.
async fn finalize(
    adapter: &Adapter,
    dev: &Device,
    cancel: &mut oneshot::Receiver<()>,
) -> Option<Address> {
    let addr = dev.address();
    let name = dev.alias().await.unwrap_or_else(|_| addr.to_string());
    if !dev.is_paired().await.unwrap_or(false) {
        println!("Pairing with {name} — approve the request on that device if prompted…");
        let mut res = pair_once(dev, cancel).await?;
        // bluetoothd drops unbonded devices some time after discovery stops
        // (`TemporaryTimeout`, 30 s by default), so a slow pick can leave us
        // pairing against an object that no longer exists ("the target object was
        // not present or removed"). Rediscover to recreate it, then retry once.
        if res.is_err()
            && !adapter
                .device_addresses()
                .await
                .unwrap_or_default()
                .contains(&addr)
        {
            info!("{name} is no longer known to bluetoothd; rescanning for it…");
            rediscover(adapter, addr, cancel).await?;
            res = pair_once(dev, cancel).await?;
        }
        if let Err(err) = res {
            warn!("pairing with {name} failed: {err}");
            return None;
        }
        info!("paired with {name}");
    }
    info!("will initiate HID connection to {name}");
    Some(addr)
}

// --- Event loop ----------------------------------------------------------

/// What the navigation loop resolved to, before the terminal is restored.
/// `Connect` still holds the live [`Device`], since the caller has to pair it;
/// the other two need only the address.
enum Chosen {
    Connect(Device),
    Fix(Address),
    Forget(Address),
}

/// The interactive loop, in raw mode. Returns what the user chose, or `None` on
/// skip/cancel.
async fn menu_loop(
    adapter: &Adapter,
    kind: Kind,
    stale: &[Address],
    known: &[Address],
    suspend_rx: &mut mpsc::Receiver<SuspendReq>,
    cancel: &mut oneshot::Receiver<()>,
) -> Option<Chosen> {
    let mut out = io::stdout();
    let mut events = EventStream::new();
    // BLE reads bonds rather than scanning, so there is nothing to wait for and
    // no reason to say so.
    let mut prev = match kind.picks_hosts() {
        true => draw_lines(&mut out, &scanning_line(), 0).ok()?,
        false => 0,
    };
    let mut state = scan(adapter, kind, stale, known, cancel).await?;
    loop {
        prev = draw_lines(&mut out, &render_lines(&state), prev).ok()?;
        tokio::select! {
            _ = &mut *cancel => return None,
            // A pairing prompt needs the terminal: release it (cooked mode) until
            // the prompt is done, then take it back and repaint (§5). The
            // `EventStream` is dropped for the duration — crossterm's reader
            // thread consumes stdin as it polls, so merely not calling
            // `events.next()` would still let it swallow the prompt's reply; only
            // dropping it stops that thread. A fresh stream is built on resume.
            Some(req) = suspend_rx.recv() => {
                drop(events);
                match suspend_for_prompt(&mut out, req, cancel).await {
                    Some(p) => {
                        prev = p;
                        events = EventStream::new();
                    }
                    None => return None,
                }
            }
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                    match on_key(&mut state, k) {
                        Action::None => {}
                        Action::Skip => return None,
                        Action::Rescan => {
                            if kind.picks_hosts() {
                                prev = draw_lines(&mut out, &scanning_line(), prev).ok()?;
                            }
                            state = scan(adapter, kind, stale, known, cancel).await?;
                        }
                        Action::Select(i) => {
                            return state.devs().get(i).cloned().map(Chosen::Connect);
                        }
                        // Fix and Forget act on the address alone, so they work
                        // for a host bluetoothd has no device object for.
                        Action::Fix(i) => {
                            return state.rows().get(i).map(|r| Chosen::Fix(r.addr));
                        }
                        Action::Forget(i) => {
                            return state.rows().get(i).map(|r| Chosen::Forget(r.addr));
                        }
                    }
                }
                Some(Ok(_)) => {}                    // resize etc.: redraw next loop
                Some(Err(_)) | None => return None,  // input closed: give up the menu
            }
        }
    }
}

/// Handle a [`SuspendReq`]: drop out of raw mode, acknowledge, and wait for the
/// pairing prompt to finish (or a cancel) before restoring raw mode. Returns the
/// redraw baseline (`0`, forcing a full repaint below the prompt) on resume, or
/// `None` if the menu was cancelled while suspended — in which case the terminal
/// is deliberately left cooked for the caller/`TermGuard` to keep.
async fn suspend_for_prompt(
    out: &mut impl Write,
    req: SuspendReq,
    cancel: &mut oneshot::Receiver<()>,
) -> Option<usize> {
    let SuspendReq { ack, resume } = req;
    let _ = terminal::disable_raw_mode();
    let _ = execute!(out, cursor::Show);
    let _ = ack.send(());
    tokio::select! {
        _ = &mut *cancel => None,     // cancelled mid-prompt: give up the menu
        _ = resume => {
            let _ = terminal::enable_raw_mode();
            Some(0)                   // full repaint (the prompt scrolled the screen)
        }
    }
}

/// Entry point: run the menu to a decision. Spawned as a task by [`Session`];
/// the returned pick (if any) is sent back over its channel.
async fn run(
    adapter: Adapter,
    kind: Kind,
    stale: Vec<Address>,
    known: Vec<Address>,
    coord: TermCoord,
    mut cancel: oneshot::Receiver<()>,
) -> Option<Pick> {
    install_panic_hook();
    // Publish this menu's suspend channel so the pairing agent can borrow the
    // terminal while the menu is up; always clear it when the menu ends.
    let mut suspend_rx = coord.register();
    // Raw mode is confined to the navigation loop; the guard drops (restoring the
    // terminal) before any pairing prompt/logs in `finalize`.
    let picked = match TermGuard::enter() {
        Ok(_guard) => menu_loop(&adapter, kind, &stale, &known, &mut suspend_rx, &mut cancel).await,
        Err(_) => None,
    };
    coord.deregister();
    match picked {
        // Only a connect has to pair first; fix and forget act on a host that is
        // bonded already, so they skip `finalize` entirely.
        Some(Chosen::Connect(dev)) => finalize(&adapter, &dev, &mut cancel)
            .await
            .map(Pick::Connect),
        Some(Chosen::Fix(addr)) => Some(Pick::Fix(addr)),
        Some(Chosen::Forget(addr)) => Some(Pick::Forget(addr)),
        None => None,
    }
}

// --- Transport-facing handle ---------------------------------------------

/// One accept cycle's menu, as the transports drive it (design/CONNECTION.md §6):
/// spawned at the top of `wait_connected` so it re-opens after a disconnect,
/// raced against the link coming up, then cancelled and joined on the way out so
/// the terminal is restored before anything else prints.
///
/// Keep this in a *local* of `wait_connected`, not a transport field: its `&mut`
/// borrow in a `select!` arm would otherwise conflict with the shared `&self`
/// borrow the concurrent accept/dial futures take.
pub struct Session {
    /// `None` once the menu has resolved (picked, skipped, or gone), which makes
    /// [`Session::recv`] pend forever thereafter.
    pick_rx: Option<mpsc::Receiver<Pick>>,
    cancel_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Session {
    /// Spawn this cycle's menu. With no adapter or outside interactive mode the
    /// result is inert: nothing is spawned and [`Session::recv`] never resolves.
    /// `stale` are the bonded hosts whose cached descriptor no longer matches
    /// (§7.1); `known` is every host blooter has a record for, which the BLE
    /// list unions with bluetoothd's bonds so nothing can drop out of view.
    /// Classic builds its list by scanning and ignores `known`.
    pub fn spawn(
        adapter: Option<&Adapter>,
        interactive: bool,
        kind: Kind,
        stale: Vec<Address>,
        known: Vec<Address>,
        coord: &TermCoord,
    ) -> Self {
        let (Some(adapter), true) = (adapter, interactive) else {
            return Self {
                pick_rx: None,
                cancel_tx: None,
                task: None,
            };
        };
        let (pick_tx, pick_rx) = mpsc::channel::<Pick>(1);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let adapter = adapter.clone();
        let coord = coord.clone();
        let task = tokio::spawn(async move {
            if let Some(pick) = run(adapter, kind, stale, known, coord, cancel_rx).await {
                let _ = pick_tx.send(pick).await;
            }
        });
        Self {
            pick_rx: Some(pick_rx),
            cancel_tx: Some(cancel_tx),
            task: Some(task),
        }
    }

    /// Whether the menu is still up, for the "incoming connection preempts the
    /// menu" note.
    pub fn is_open(&self) -> bool {
        self.pick_rx.is_some()
    }

    /// Resolve once with the menu's outcome — `Some(pick)`, or `None` if it was
    /// skipped or its sender was dropped — then pend forever, so this can sit in
    /// a `select!` loop that keeps running afterwards.
    pub async fn recv(&mut self) -> Option<Pick> {
        match &mut self.pick_rx {
            Some(rx) => {
                let picked = rx.recv().await;
                self.pick_rx = None;
                picked
            }
            None => future::pending().await,
        }
    }

    /// Preempt the menu and wait for it to restore the terminal. Idempotent, so
    /// it can be called on every exit path.
    pub async fn finish(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.pick_rx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn addr(n: u8) -> Address {
        Address::new([0, 0, 0, 0, 0, n])
    }

    fn row(name: &str, connected: bool, paired: bool, rssi: Option<i16>) -> Row {
        Row {
            stale: false,
            addr: addr(1),
            alias: name.to_string(),
            connected,
            paired,
            rssi,
            forgotten_by_bluez: false,
        }
    }

    fn state(main: Vec<Row>, other: Vec<Row>) -> MenuState {
        state_of(Kind::Classic, main, other)
    }

    fn state_of(kind: Kind, main: Vec<Row>, other: Vec<Row>) -> MenuState {
        MenuState {
            kind,
            screen: Screen::Main,
            main,
            other,
            main_devs: Vec::new(),
            other_devs: Vec::new(),
            selected: 0,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Classes observed on real devices, rather than values invented to match
    /// the code: every one of these was read off a live `org.bluez.Device1`.
    /// The Audio/Video major class (4) is the interesting one — it holds both
    /// the headsets and the TVs, and the TVs disagree wildly about their minor
    /// class, which is why classification must not allow-list known-good ones.
    #[test]
    fn classify_real_world_classes() {
        // (class, expected `is_other`, the device it was read from)
        let cases = [
            (0x006c_010c, false, "garmak: laptop, A2DP bit set"),
            (0x002e_410c, false, "PC-673942: desktop, major 1"),
            (0x005a_020c, false, "Galaxy A56: phone, major 2"),
            (0x001c_0424, false, "Telia TV: set-top box, minor 0x09"),
            (0x0008_043c, false, "Samsung LED55: display, minor 0x0f"),
            (0x000c_243c, false, "LG webOS TV: display, minor 0x0f"),
            // The regression this deny-list exists for: a TV claiming a minor
            // class ("car audio") no allow-list of displays would include.
            (0x002c_0420, false, "GoogleTV6946: car audio, minor 0x08"),
            (0x0024_0404, true, "Core Wireless Pods: earbuds, 0x01"),
            (0x0024_0414, true, "MD 43402: loudspeaker, minor 0x05"),
        ];
        for (class, want, what) in cases {
            assert_eq!(
                is_other(Some(class), None, true, false),
                want,
                "{what} ({class:#08x})"
            );
        }
    }

    #[test]
    fn classify_peripheral_major_classes_are_other() {
        // Major class 5 is Peripheral — another keyboard (0x40) or mouse (0x80)
        // is not a host to connect *to*.
        assert!(is_other(Some(0x0000_0540), None, true, false));
        assert!(is_other(Some(0x0000_0580), None, true, false));
        // Nor is a printer (6), a smartwatch (7), a toy (8) or a scale (9).
        assert!(is_other(Some(0x0000_0680), None, true, false));
        assert!(is_other(Some(0x0000_0704), None, true, false));
        assert!(is_other(Some(0x0000_0804), None, true, false));
        assert!(is_other(Some(0x0000_0904), None, true, false));
    }

    #[test]
    fn classify_unknown_classes_stay_on_main() {
        // The deny-list bias: only a recognised peripheral is demoted, so an
        // uncategorised (major 31) or miscellaneous (major 0) device — common on
        // cheap TV boxes — stays visible, A2DP bit and all.
        assert!(!is_other(Some(0x0020_1f00), None, true, false));
        assert!(!is_other(Some(0x0020_0000), None, true, false));
        // Including the bare Audio service bit, which every host sets too.
        assert!(!is_other(Some(1 << 21), None, true, false));
    }

    #[test]
    fn classify_no_name_is_other() {
        assert!(is_other(Some(0x0000_010c), None, false, false));
        assert!(is_other(None, None, false, false));
    }

    #[test]
    fn classify_paired_devices_stay_on_main() {
        // A bond beats every other signal, including a missing name.
        assert!(!is_other(Some(0x0024_0404), Some(0x0841), false, true));
        assert!(!is_other(None, None, false, true));
    }

    #[test]
    fn classify_named_hosts_are_main() {
        // Computer (major 1) and phone (major 2), and an unknown class but named.
        assert!(!is_other(Some(0x0000_010c), None, true, false)); // major 1 computer
        assert!(!is_other(Some(0x0000_020c), None, true, false)); // major 2 phone
        assert!(!is_other(None, None, true, false));
    }

    #[test]
    fn classify_by_appearance_when_le_only() {
        // LE-only peers carry no Class of Device, so the GAP Appearance decides.
        // Keyboard (0x03C1) and mouse (0x03C2) are category 0x0F (HID); a
        // standalone speaker (0x0841) is category 0x21 (Audio Sink).
        assert!(is_other(None, Some(0x03C1), true, false));
        assert!(is_other(None, Some(0x03C2), true, false));
        assert!(is_other(None, Some(0x0841), true, false));
        // Generic Computer (0x0080) and Generic Phone (0x0040) are hosts, and so
        // is anything with an unknown/unset appearance.
        assert!(!is_other(None, Some(0x0080), true, false));
        assert!(!is_other(None, Some(0x0040), true, false));
        assert!(!is_other(None, Some(0x0000), true, false));
    }

    #[test]
    fn arrows_respect_bounds() {
        let mut s = state(
            vec![row("a", false, false, None), row("b", false, false, None)],
            vec![],
        );
        assert_eq!(on_key(&mut s, key(KeyCode::Up)), Action::None);
        assert_eq!(s.selected, 0); // no underflow
        on_key(&mut s, key(KeyCode::Down));
        assert_eq!(s.selected, 1);
        on_key(&mut s, key(KeyCode::Down));
        assert_eq!(s.selected, 1); // no overflow past last
    }

    #[test]
    fn number_keys_select_in_range_only() {
        let mut s = state(
            vec![row("a", false, false, None), row("b", false, false, None)],
            vec![],
        );
        assert_eq!(on_key(&mut s, key(KeyCode::Char('2'))), Action::Select(1));
        assert_eq!(on_key(&mut s, key(KeyCode::Char('3'))), Action::None);
    }

    #[test]
    fn enter_selects_cursor_or_skips_when_empty() {
        let mut s = state(vec![row("a", false, false, None)], vec![]);
        assert_eq!(on_key(&mut s, key(KeyCode::Enter)), Action::Select(0));
        let mut empty = state(vec![], vec![]);
        assert_eq!(on_key(&mut empty, key(KeyCode::Enter)), Action::Skip);
    }

    #[test]
    fn other_submenu_open_and_back() {
        let mut s = state(vec![], vec![row("hs", false, true, None)]);
        on_key(&mut s, key(KeyCode::Char('o')));
        assert_eq!(s.screen, Screen::Other);
        // On the Other screen numbers select from `other`.
        assert_eq!(on_key(&mut s, key(KeyCode::Char('1'))), Action::Select(0));
        on_key(&mut s, key(KeyCode::Esc));
        assert_eq!(s.screen, Screen::Main);
    }

    #[test]
    fn open_other_ignored_when_empty() {
        let mut s = state(vec![row("a", false, false, None)], vec![]);
        on_key(&mut s, key(KeyCode::Char('o')));
        assert_eq!(s.screen, Screen::Main);
    }

    #[test]
    fn action_keys() {
        let mut s = state(vec![row("a", false, false, None)], vec![]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('r'))), Action::Rescan);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('q'))), Action::Skip);
        assert_eq!(on_key(&mut s, key(KeyCode::Esc)), Action::Skip); // Esc on Main skips
    }

    #[test]
    fn render_marks_selection_and_footer_count() {
        let mut s = state(
            vec![row("Laptop", false, true, Some(-45))],
            vec![row("Headset", false, false, None)],
        );
        s.selected = 0;
        let lines = render_lines(&s);
        assert_eq!(lines[0], "Bluetooth hosts:");
        assert!(
            lines[1].starts_with("> 1."),
            "selected row marked: {}",
            lines[1]
        );
        assert!(lines[1].contains("Laptop"));
        assert!(lines[1].contains("[paired, -45 dBm]"));
        // The cursor is on a bonded host, so `[f]` is offered.
        assert_eq!(
            lines.last().unwrap(),
            "[o] Other devices (1)   [f] Fix connection   [r] Rescan   [q] Skip"
        );
    }

    #[test]
    fn render_footer_without_other_devices() {
        let s = state(vec![row("Laptop", true, true, None)], vec![]);
        let lines = render_lines(&s);
        assert_eq!(
            lines.last().unwrap(),
            "[f] Fix connection   [r] Rescan   [q] Skip"
        );
        assert!(lines[1].contains("[connected]"));
    }

    #[test]
    fn fix_offered_only_for_bonded_hosts() {
        // Unpaired: the key is inert and the footer does not advertise it.
        let mut s = state(vec![row("Phone", false, false, None)], vec![]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('f'))), Action::None);
        assert!(!render_lines(&s).last().unwrap().contains("[f]"));
        // Paired: the key fixes the selected row.
        let mut s = state(
            vec![
                row("Phone", false, false, None),
                row("Laptop", false, true, None),
            ],
            vec![],
        );
        s.selected = 1;
        assert_eq!(on_key(&mut s, key(KeyCode::Char('f'))), Action::Fix(1));
    }

    #[test]
    fn stale_hosts_are_marked_and_explained() {
        let mut r = row("Laptop", false, true, None);
        r.stale = true;
        let s = state(vec![r], vec![]);
        let lines = render_lines(&s);
        assert!(lines[1].contains("[paired, stale]"), "{}", lines[1]);
        assert!(
            lines
                .last()
                .unwrap()
                .contains("cached an older HID descriptor")
        );
        // Not stale: no marker, no explanation line.
        let s = state(vec![row("Laptop", false, true, None)], vec![]);
        let lines = render_lines(&s);
        assert!(lines[1].contains("[paired]"));
        assert!(!lines.last().unwrap().contains("cached an older"));
    }

    // --- The BLE menu: a bonded-host manager, not a host picker (§6) ------

    fn ble(main: Vec<Row>) -> MenuState {
        state_of(Kind::Ble, main, vec![])
    }

    /// Selecting means "connect to this", and a BLE peripheral cannot connect
    /// to anything. Number keys therefore only move the cursor, and Enter
    /// closes the menu rather than picking.
    #[test]
    fn ble_never_selects_a_host_to_connect_to() {
        let mut s = ble(vec![
            row("Laptop", false, true, None),
            row("TV", false, true, None),
        ]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('2'))), Action::None);
        assert_eq!(s.selected, 1, "the number key still moves the cursor");
        assert_eq!(on_key(&mut s, key(KeyCode::Enter)), Action::Skip);
        // Classic, for contrast, picks with both.
        let mut s = state(vec![row("Laptop", false, true, None)], vec![]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('1'))), Action::Select(0));
        assert_eq!(on_key(&mut s, key(KeyCode::Enter)), Action::Select(0));
    }

    /// `[u]` is the only way a bond is ever dropped on BLE, so it must be
    /// offered there and nowhere else — a failed `[f]` must not do it silently
    /// (§7.2b).
    #[test]
    fn forget_is_offered_on_ble_for_bonded_hosts_only() {
        let mut s = ble(vec![row("Laptop", false, true, None)]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('u'))), Action::Forget(0));
        assert!(render_lines(&s).last().unwrap().contains("[u] Forget host"));
        // Not bonded: nothing to forget.
        let mut s = ble(vec![row("Laptop", false, false, None)]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('u'))), Action::None);
        assert!(!render_lines(&s).last().unwrap().contains("[u]"));
        // Classic drops the bond as part of `[f]`'s unplug, so it has no `[u]`.
        let mut s = state(vec![row("Laptop", false, true, None)], vec![]);
        assert_eq!(on_key(&mut s, key(KeyCode::Char('u'))), Action::None);
        assert!(!render_lines(&s).last().unwrap().contains("[u]"));
    }

    /// There is no scan, so there is no "Other devices" list to classify into
    /// or navigate to.
    #[test]
    fn ble_has_no_other_devices_submenu() {
        let mut s = ble(vec![row("Laptop", false, true, None)]);
        on_key(&mut s, key(KeyCode::Char('o')));
        assert_eq!(s.screen, Screen::Main);
        let footer = render_lines(&s).last().unwrap().clone();
        assert!(!footer.contains("[o]"), "{footer}");
        assert!(footer.contains("[r] Refresh"), "{footer}");
        assert!(footer.contains("[q] Close"), "{footer}");
    }

    /// The header has to say what the user is meant to do, because on BLE the
    /// answer is "go to the host" — nothing in this menu will pair for them.
    #[test]
    fn ble_header_directs_pairing_to_the_host() {
        let lines = render_lines(&ble(vec![]));
        assert!(
            lines[0].contains("pair new ones from the host"),
            "{lines:?}"
        );
        assert_eq!(lines[1], "  (none yet)");
    }

    /// A host bluetoothd has dropped the object for still gets a row: it is the
    /// only place left to act on it, since it can never be rediscovered.
    #[test]
    fn ble_marks_hosts_bluez_no_longer_knows() {
        let mut r = row("00:00:00:00:00:01", false, true, None);
        r.forgotten_by_bluez = true;
        let s = ble(vec![r]);
        let lines = render_lines(&s);
        assert!(lines[1].contains("unknown to bluetoothd"), "{}", lines[1]);
    }

    /// The BLE repair keeps the bond and needs a live link, so the advice
    /// differs from Classic's "re-pair afterwards".
    #[test]
    fn ble_stale_advice_asks_for_a_connection_not_a_re_pair() {
        let mut r = row("Laptop", false, true, None);
        r.stale = true;
        let advice = render_lines(&ble(vec![r])).last().unwrap().clone();
        assert!(
            advice.contains("connect from it, then press [f]"),
            "{advice}"
        );
        assert!(!advice.contains("re-pair"), "{advice}");
    }
}
