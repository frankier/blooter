//! Interactive host-(re)connection menu (design/CONNECTION.md §6).
//!
//! A small, pre-emptable TUI built directly on `crossterm`'s async
//! [`EventStream`]: arrow keys move, number keys pick a host, letter keys drive
//! actions ("Other devices" submenu, rescan, skip). It runs as a spawned task
//! that the Classic transport races against an incoming connection; a `oneshot`
//! cancel signal (fired on inbound-accept or shutdown) preempts the menu at any
//! await point and the terminal is always restored.
//!
//! Ineligible devices — Bluetooth audio/headsets, and devices with no real name
//! (only a hex identifier) — are moved to an "Other devices" submenu so the main
//! list shows just plausible HID hosts (computers/phones).

use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use bluer::{Adapter, AdapterEvent, Address, Device};
use crossterm::cursor;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use futures::StreamExt;
use log::{info, warn};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

/// How long to scan on entry and on each rescan.
const SCAN_SECS: u64 = 4;

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
    rssi: Option<i16>,
    /// Bonded under a different HID report descriptor than the current one, so
    /// this host is still using a cached copy that no longer matches what
    /// blooter sends (design/CONNECTION.md §7). Fixable with `[f]`.
    stale: bool,
}

/// What the menu resolved to: a host, and what to do with it.
pub struct Pick {
    pub addr: Address,
    /// Unplug and unbond this host instead of connecting to it, so its next
    /// pairing re-reads blooter's SDP record (design/CONNECTION.md §7).
    pub fix: bool,
}

/// What a keypress asks the event loop to do. Cursor moves and screen switches
/// are applied to the state in place and reported as `None` (the loop redraws
/// every iteration regardless).
#[derive(PartialEq, Eq, Debug)]
enum Action {
    None,
    /// Select the device at this index within the active screen's list.
    Select(usize),
    /// Fix the connection to the device at this index: unplug + unbond it.
    Fix(usize),
    Rescan,
    Skip,
}

struct MenuState {
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

/// True if a device belongs in the "Other devices" submenu rather than the main
/// host list. Strict rule, applied uniformly (even to paired/connected devices):
/// a device is "Other" if it has no real name, or its Class of Device marks it
/// as audio (headset/speaker/etc.). A named device with an unknown class stays
/// in the main list.
fn is_other(class: Option<u32>, has_real_name: bool) -> bool {
    if !has_real_name {
        return true;
    }
    match class {
        // Major device class (bits 8-12) == 4 is Audio/Video; the Audio service
        // class flag is bit 21. Either marks a headset/speaker-type device.
        Some(c) => ((c >> 8) & 0x1f) == 4 || (c & (1 << 21)) != 0,
        None => false,
    }
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
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if idx < len {
                Action::Select(idx)
            } else {
                Action::None
            }
        }
        KeyCode::Enter => {
            if len > 0 {
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
    let (title, rows) = match state.screen {
        Screen::Main => ("Bluetooth hosts:", &state.main),
        Screen::Other => ("Other devices:", &state.other),
    };
    lines.push(title.to_string());
    if rows.is_empty() {
        lines.push("  (none found)".to_string());
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
        lines.push(format!(
            "{marker} {}. {}  {} [{st}{sig}{stale}]",
            i + 1,
            r.addr,
            r.alias
        ));
    }
    lines.push(String::new());
    // `[f]` applies to bonded hosts only, so it is offered only when the cursor
    // is on one.
    let fix = match rows.get(state.selected) {
        Some(r) if r.paired => "[f] Fix connection   ",
        _ => "",
    };
    let footer = match state.screen {
        Screen::Main if state.other.is_empty() => format!("{fix}[r] Rescan   [q] Skip"),
        Screen::Main => format!(
            "[o] Other devices ({})   {fix}[r] Rescan   [q] Skip",
            state.other.len()
        ),
        Screen::Other => format!("[b] Back   {fix}[r] Rescan   [q] Skip"),
    };
    lines.push(footer);
    if rows.iter().any(|r| r.stale) {
        lines.push(
            "A host marked 'stale' cached an older HID descriptor and will not see \
             blooter's current one; [f] fixes it (re-pair afterwards)."
                .to_string(),
        );
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
        };
        if is_other(class, has_real_name) {
            other.push((row, dev));
        } else {
            main.push((row, dev));
        }
    }
    (main, other)
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

/// Run a short discovery pass (cancellable), then build a fresh [`MenuState`].
/// Returns `None` only if cancelled mid-scan.
async fn scan(
    adapter: &Adapter,
    stale: &[Address],
    cancel: &mut oneshot::Receiver<()>,
) -> Option<MenuState> {
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

/// The interactive loop, in raw mode. Returns the chosen [`Device`] (to be paired
/// by the caller after the terminal is restored), or `None` on skip/cancel.
async fn menu_loop(
    adapter: &Adapter,
    stale: &[Address],
    suspend_rx: &mut mpsc::Receiver<SuspendReq>,
    cancel: &mut oneshot::Receiver<()>,
) -> Option<(Device, bool)> {
    let mut out = io::stdout();
    let mut events = EventStream::new();
    let mut prev = draw_lines(&mut out, &scanning_line(), 0).ok()?;
    let mut state = scan(adapter, stale, cancel).await?;
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
                            prev = draw_lines(&mut out, &scanning_line(), prev).ok()?;
                            state = scan(adapter, stale, cancel).await?;
                        }
                        Action::Select(i) => {
                            return state.devs().get(i).cloned().map(|d| (d, false));
                        }
                        Action::Fix(i) => {
                            return state.devs().get(i).cloned().map(|d| (d, true));
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

/// Entry point: run the menu to a decision. Spawned as a task by the Classic
/// transport; the returned address (if any) is sent back over its channel.
pub async fn run(
    adapter: Adapter,
    stale: Vec<Address>,
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
        Ok(_guard) => menu_loop(&adapter, &stale, &mut suspend_rx, &mut cancel).await,
        Err(_) => None,
    };
    coord.deregister();
    match picked {
        // A fix targets an already-bonded host and deliberately tears the bond
        // down, so it skips `finalize`'s pairing entirely.
        Some((dev, true)) => Some(Pick {
            addr: dev.address(),
            fix: true,
        }),
        Some((dev, false)) => finalize(&adapter, &dev, &mut cancel)
            .await
            .map(|addr| Pick { addr, fix: false }),
        None => None,
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
        }
    }

    fn state(main: Vec<Row>, other: Vec<Row>) -> MenuState {
        MenuState {
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

    #[test]
    fn classify_audio_major_class() {
        // Major class 4 (Audio/Video), e.g. a headset.
        assert!(is_other(Some(0x0024_0404), true));
    }

    #[test]
    fn classify_audio_service_bit() {
        assert!(is_other(Some(1 << 21), true));
    }

    #[test]
    fn classify_no_name_is_other() {
        assert!(is_other(Some(0x0000_010c), false));
        assert!(is_other(None, false));
    }

    #[test]
    fn classify_named_hosts_are_main() {
        // Computer (major 1) and phone (major 2), and an unknown class but named.
        assert!(!is_other(Some(0x0000_010c), true)); // major 1 computer
        assert!(!is_other(Some(0x0000_020c), true)); // major 2 phone
        assert!(!is_other(None, true));
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
}
