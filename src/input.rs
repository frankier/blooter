//! Input sources: evdev event devices (default) and FIFO mode. See design/ARCH.md §6.

use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};
use log::{debug, info, warn};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, watch};

use crate::keymap;
use crate::report::RawEvent;

const MAX_EVDEVS: u32 = 64;

/// Shared registry of live hotplugged gamepad reader task handles. Grows as the
/// udev monitor opens controllers at runtime; drained on shutdown.
#[derive(Clone, Default)]
struct DynHandles(Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>);

impl DynHandles {
    fn push(&self, handle: tokio::task::JoinHandle<()>) {
        self.0.lock().unwrap().push(handle);
    }

    fn take(&self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// Handle keeping input reader tasks/threads alive. Dropping it (or aborting on
/// shutdown) closes the device fds, which releases any exclusive grab.
pub struct Inputs {
    /// Startup readers plus the FIFO reader and the udev hotplug monitor task.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Readers for gamepads opened later by the hotplug monitor.
    dynamic: DynHandles,
}

impl Drop for Inputs {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        for t in self.dynamic.take() {
            t.abort();
        }
    }
}

impl Inputs {
    /// Graceful shutdown. Once the capture watch channel is closed, each evdev
    /// reader releases its grab (injecting the touchpad reset) and exits; wait
    /// briefly for that. Anything still running afterwards is aborted. Covers
    /// both the startup readers and any hotplugged ones.
    ///
    /// Every reader here is a plain async task, which is what makes the abort
    /// mean anything: `abort` does nothing to a running `spawn_blocking`
    /// closure, so a reader on its own thread would survive this and then hold
    /// the runtime open at exit (see `spawn_fifo`).
    pub async fn shutdown(mut self) {
        let mut all = std::mem::take(&mut self.tasks);
        all.extend(self.dynamic.take());
        for mut t in all {
            if tokio::time::timeout(std::time::Duration::from_millis(250), &mut t)
                .await
                .is_err()
            {
                t.abort();
            }
        }
    }
}

fn event_path(num: u32) -> PathBuf {
    PathBuf::from(format!("/dev/input/event{num}"))
}

/// The event-device numbers to try, in ascending order: an explicit `-e`
/// selection as given, otherwise the full default-scan range.
fn selected_numbers(event_devices: &[u32]) -> Vec<u32> {
    if event_devices.is_empty() {
        (0..MAX_EVDEVS).collect()
    } else {
        event_devices.to_vec()
    }
}

/// Whether the default scan should pick up this device: anything that looks
/// like a keyboard, a relative pointer (mouse/trackpoint) or a touchpad.
/// Filters out power buttons, lid switches, jack-detect devices and the like.
/// Gamepads are handled separately (see `is_gamepad`).
fn is_relevant(dev: &Device) -> bool {
    let Some(keys) = dev.supported_keys() else {
        return false;
    };
    let evs = dev.supported_events();
    (evs.contains(EventType::RELATIVE) && keys.contains(KeyCode::BTN_LEFT))
        || (evs.contains(EventType::ABSOLUTE) && keys.contains(KeyCode::BTN_TOUCH))
        || keys.contains(KeyCode::KEY_A)
}

/// Whether this device is a gamepad or joystick: absolute axes plus at least
/// one button in the joystick/gamepad button range (`BTN_JOYSTICK` 0x120 ..=
/// `BTN_THUMBR` 0x13f). This mirrors udev/libinput's `ID_INPUT_JOYSTICK`
/// heuristic and covers both modern pads (which report the `BTN_GAMEPAD` range
/// starting at `BTN_SOUTH` 0x130) and cheap/generic pads, flight sticks and
/// arcade sticks (which report the `BTN_JOYSTICK` range: `BTN_TRIGGER`,
/// `BTN_THUMB`, …). Touchpads (`BTN_TOUCH` 0x14a) and styli (`BTN_STYLUS`
/// 0x14b) sit above the range and are not misclassified.
fn is_gamepad(dev: &Device) -> bool {
    let Some(keys) = dev.supported_keys() else {
        return false;
    };
    dev.supported_events().contains(EventType::ABSOLUTE)
        && (0x120..=0x13f).any(|c| keys.contains(KeyCode::new(c)))
}

/// Count the gamepads among the selected devices, in the same ascending order
/// `spawn` assigns slots. Used before profile registration to size the HID
/// descriptor (report IDs 3, 4, …).
pub fn count_gamepads(event_devices: &[u32]) -> usize {
    selected_numbers(event_devices)
        .into_iter()
        .filter(|&num| {
            Device::open(event_path(num))
                .map(|d| is_gamepad(&d))
                .unwrap_or(false)
        })
        .count()
}

/// Capture the min/max range of each gamepad stick/trigger axis (`ABS_X`..
/// `ABS_RZ`), for normalizing raw axis values to the HID report's 0..=255.
fn gamepad_abs_ranges(dev: &Device) -> [Option<(i32, i32)>; 6] {
    let mut ranges = [None; 6];
    if let Ok(absinfos) = dev.get_absinfo() {
        for (code, info) in absinfos {
            let c = code.0;
            if keymap::is_stick_or_trigger(c) {
                ranges[c as usize] = Some((info.minimum(), info.maximum()));
            }
        }
    }
    ranges
}

/// Scale a raw axis value from `[min, max]` to the HID report's 0..=255.
fn normalize_axis(value: i32, min: i32, max: i32) -> i32 {
    if max <= min {
        return value.clamp(0, 255);
    }
    ((i64::from(value - min) * 255) / i64::from(max - min)).clamp(0, 255) as i32
}

/// For a touchpad, the neutral "all fingers up" sequence injected (written
/// back to the device node) right after releasing an exclusive grab, so
/// libinput resumes from a known state rather than a stale mid-gesture one.
/// The kernel input core drops events that do not change device state, so the
/// injection is a no-op when the state is already clean. `None` for
/// non-touch devices.
fn touch_reset_events(dev: &Device) -> Option<Vec<InputEvent>> {
    let keys = dev.supported_keys()?;
    if !dev.supported_events().contains(EventType::ABSOLUTE) || !keys.contains(KeyCode::BTN_TOUCH) {
        return None;
    }
    let mut evs = Vec::new();
    // Empty every multitouch slot (tracking id -1 = no contact).
    let slots = dev
        .get_absinfo()
        .ok()
        .and_then(|mut it| it.find(|(c, _)| *c == AbsoluteAxisCode::ABS_MT_SLOT))
        .map_or(0, |(_, info)| info.maximum() + 1);
    for slot in 0..slots {
        evs.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_SLOT.0,
            slot,
        ));
        evs.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
            -1,
        ));
    }
    for code in [
        KeyCode::BTN_TOUCH,
        KeyCode::BTN_TOOL_FINGER,
        KeyCode::BTN_TOOL_DOUBLETAP,
        KeyCode::BTN_TOOL_TRIPLETAP,
        KeyCode::BTN_TOOL_QUADTAP,
        KeyCode::BTN_TOOL_QUINTTAP,
    ] {
        if keys.contains(code) {
            evs.push(InputEvent::new(EventType::KEY.0, code.0, 0));
        }
    }
    evs.push(InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0));
    Some(evs)
}

/// Tracks which of the advertised gamepad slots (`0..count`) are occupied, so
/// hotplugged controllers can claim a free slot and reader tasks release theirs
/// on exit. An atomic bitmask keeps claim/free lock-free and race-free.
#[derive(Clone)]
struct SlotPool {
    inner: Arc<SlotPoolInner>,
}

struct SlotPoolInner {
    /// Bit `i` set ⇒ slot `i` is in use.
    occupied: AtomicU64,
    /// Number of advertised slots (capped at 64, the bitmask width).
    count: u32,
}

impl SlotPool {
    fn new(count: usize) -> Self {
        SlotPool {
            inner: Arc::new(SlotPoolInner {
                occupied: AtomicU64::new(0),
                count: count.min(64) as u32,
            }),
        }
    }

    /// Claim the lowest free slot, or `None` if all `count` slots are taken.
    /// Two concurrent callers can never receive the same slot.
    fn claim(&self) -> Option<u8> {
        let mask = if self.inner.count >= 64 {
            u64::MAX
        } else {
            (1u64 << self.inner.count) - 1
        };
        let mut cur = self.inner.occupied.load(Ordering::Relaxed);
        loop {
            let free = !cur & mask;
            if free == 0 {
                return None;
            }
            let bit = free.trailing_zeros();
            match self.inner.occupied.compare_exchange_weak(
                cur,
                cur | (1u64 << bit),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(bit as u8),
                Err(observed) => cur = observed,
            }
        }
    }

    /// Release a slot back to the pool (idempotent).
    fn free(&self, slot: u8) {
        self.inner
            .occupied
            .fetch_and(!(1u64 << slot), Ordering::AcqRel);
    }
}

/// Frees a gamepad's slot when its reader task ends, by any path (read error,
/// unplug/EOF, capture-channel close, or task abort during shutdown).
struct SlotReleaser {
    pool: SlotPool,
    slot: u8,
}

impl Drop for SlotReleaser {
    fn drop(&mut self) {
        self.pool.free(self.slot);
    }
}

/// Everything needed to open a device and spawn its reader task. Cloneable so
/// the udev hotplug monitor can build new readers on the fly.
#[derive(Clone)]
struct ReaderCtx {
    grab: bool,
    debug: bool,
    pool: SlotPool,
    capture: watch::Receiver<bool>,
    tx: mpsc::Sender<RawEvent>,
    /// When set (an explicit `-e` selection), hotplug only opens nodes whose
    /// event number is listed; `None` is the default scan (any node eligible).
    hotplug_filter: Option<Arc<[u32]>>,
}

/// Which discovery path is opening a device — this governs the relevance
/// filter and how open failures are reported.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Default startup scan: take keyboards/pointers/touchpads/gamepads only;
    /// silently skip unopenable nodes.
    Default,
    /// Explicit `-e` selection at startup: honour the node as-is; warn if it
    /// cannot be opened.
    Explicit,
    /// Runtime hotplug: only gamepads are of interest; silently skip the rest.
    Hotplug,
}

/// Open all configured input sources and spawn readers feeding `tx`.
/// `capture` carries the input-capture state: under `-x`, the exclusive grab
/// is released while capture is off and reacquired when it comes back on.
/// When `hotplug` is set, a udev monitor also opens gamepads plugged in later
/// into free slots. Returns an error if no usable input source could be opened.
#[allow(clippy::too_many_arguments)] // one call site; all args come straight from main
pub fn spawn(
    event_devices: &[u32],
    fifo: Option<&str>,
    grab: bool,
    debug: bool,
    gamepad_slots: usize,
    hotplug: bool,
    capture: watch::Receiver<bool>,
    tx: mpsc::Sender<RawEvent>,
) -> io::Result<Inputs> {
    if let Some(path) = fifo {
        let task = spawn_fifo(path, debug, tx)?;
        return Ok(Inputs {
            tasks: vec![task],
            dynamic: DynHandles::default(),
        });
    }

    let explicit = !event_devices.is_empty();
    let ctx = ReaderCtx {
        grab,
        debug,
        pool: SlotPool::new(gamepad_slots),
        capture,
        tx,
        hotplug_filter: explicit.then(|| Arc::from(event_devices.to_vec().into_boxed_slice())),
    };

    // Gamepads are assigned consecutive slots (report IDs 3, 4, …) in ascending
    // device order, matching `count_gamepads`: the scan is sequential and
    // `claim` always returns the lowest free slot.
    let mut tasks = Vec::new();
    let scan = if explicit {
        Scan::Explicit
    } else {
        Scan::Default
    };
    for num in selected_numbers(event_devices) {
        if let Some(t) = open_and_spawn(&ctx, num, scan) {
            tasks.push(t);
        }
    }

    let dynamic = DynHandles::default();
    if hotplug && gamepad_slots > 0 {
        match spawn_monitor(ctx.clone(), dynamic.clone()) {
            Ok(t) => tasks.push(t),
            Err(e) => warn!("gamepad hotplug disabled: cannot start udev monitor: {e}"),
        }
    }

    if tasks.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no usable input devices could be opened",
        ));
    }

    Ok(Inputs { tasks, dynamic })
}

/// Open one `/dev/input/event<num>` node and, if it is wanted under `scan`,
/// grab it (under `-x`), build its event stream and spawn its reader task.
/// Gamepads claim a free advertised slot; the task frees it on exit. Returns
/// the task handle, or `None` if the device was skipped or could not be opened.
fn open_and_spawn(ctx: &ReaderCtx, num: u32, scan: Scan) -> Option<tokio::task::JoinHandle<()>> {
    let path = event_path(num);
    let dev = match Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            // For an explicit -e selection, surface the failure; otherwise
            // silently skip unopenable devices.
            if scan == Scan::Explicit {
                warn!("cannot open {}: {e}", path.display());
            }
            return None;
        }
    };

    let gamepad = is_gamepad(&dev);

    // The default scan takes every relevant device (keyboards, pointers,
    // touchpads, gamepads); hotplug fills only gamepad slots; an explicit -e
    // selection is honoured as-is.
    match scan {
        Scan::Default if !is_relevant(&dev) && !gamepad => {
            debug!(
                "skipping {}: not a keyboard/pointer/touchpad/gamepad",
                path.display()
            );
            return None;
        }
        Scan::Hotplug if !gamepad => return None,
        _ => {}
    }

    // Each gamepad claims a free advertised slot (report ID); gamepads with no
    // free slot have no report ID and are left alone. The releaser frees the
    // slot when the reader task ends.
    let (gamepad_slot, abs_ranges, releaser) = if gamepad {
        match ctx.pool.claim() {
            Some(slot) => (
                Some(slot),
                gamepad_abs_ranges(&dev),
                Some(SlotReleaser {
                    pool: ctx.pool.clone(),
                    slot,
                }),
            ),
            None => {
                debug!("skipping gamepad {}: no free slot", path.display());
                return None;
            }
        }
    } else {
        (None, [None; 6], None)
    };

    let name = dev.name().unwrap_or("<unknown>").to_string();
    match gamepad_slot {
        Some(slot) => info!(
            "opened {} ('{}') as gamepad {}",
            path.display(),
            name,
            slot + 1
        ),
        None => info!("opened {} ('{}')", path.display(), name),
    }

    let grab = ctx.grab;
    let debug = ctx.debug;
    let mut dev = dev;
    let touch_reset = if grab { touch_reset_events(&dev) } else { None };
    let mut grabbed = false;
    // Only grab at startup if capture is already on (a device hotplugged during
    // an active session). While idle — including the whole interactive menu /
    // local session before the first connection — the keyboard stays ungrabbed
    // so the TTY receives keystrokes; the capture-change handler below grabs
    // once a host connects (design/ARCH.md §6).
    if grab && *ctx.capture.borrow() {
        match dev.grab() {
            Ok(()) => grabbed = true,
            Err(e) => warn!("could not grab {}: {e} (continuing)", path.display()),
        }
    }

    let mut stream = match dev.into_event_stream() {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot stream {}: {e}", path.display());
            return None;
        }
    };

    let tx = ctx.tx.clone();
    let devname = name;
    let mut capture_rx = ctx.capture.clone();
    Some(tokio::spawn(async move {
        // Held for the task's lifetime; its drop frees the gamepad slot.
        let _releaser = releaser;
        loop {
            tokio::select! {
                ev = stream.next_event() => match ev {
                    Ok(ev) => {
                        let type_ = ev.event_type().0;
                        let code = ev.code();
                        let mut value = ev.value();
                        // Gamepad sticks/triggers are scaled to the report's
                        // 0..=255 here, where the device's axis ranges are
                        // known; the hat and buttons pass through unchanged.
                        if gamepad_slot.is_some()
                            && type_ == EventType::ABSOLUTE.0
                            && keymap::is_stick_or_trigger(code)
                            && let Some((min, max)) = abs_ranges[code as usize]
                        {
                            value = normalize_axis(value, min, max);
                        }
                        let raw = RawEvent { type_, code, value, gamepad: gamepad_slot };
                        if debug {
                            debug!("[{devname}] type={} code={} value={}", raw.type_, raw.code, raw.value);
                        }
                        if tx.send(raw).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("read error on '{devname}': {e}");
                        break;
                    }
                },
                changed = capture_rx.changed() => {
                    // Capture off releases the exclusive grab so input
                    // reaches the local session again; on re-grabs. A
                    // closed channel means shutdown: release the grab
                    // cleanly, then exit (closing the fd).
                    let shutdown = changed.is_err();
                    let want = !shutdown && *capture_rx.borrow();
                    if grab {
                        let dev = stream.device_mut();
                        if want && !grabbed {
                            match dev.grab() {
                                Ok(()) => grabbed = true,
                                Err(e) => warn!("could not re-grab '{devname}': {e}"),
                            }
                        } else if !want && grabbed {
                            match dev.ungrab() {
                                Ok(()) => {
                                    grabbed = false;
                                    // Nudge libinput back to a clean touch
                                    // state (must come after the ungrab:
                                    // while grabbed, injected events reach
                                    // only the grab holder).
                                    if let Some(evs) = &touch_reset
                                        && let Err(e) = dev.send_events(evs)
                                    {
                                        warn!("could not reset touch state on '{devname}': {e}");
                                    }
                                }
                                Err(e) => warn!("could not ungrab '{devname}': {e}"),
                            }
                        }
                    }
                    if shutdown {
                        break;
                    }
                }
            }
        }
    }))
}

/// Spawn the udev monitor task: watch the `input` subsystem for gamepads
/// plugged in at runtime and open each into a free slot (via `open_and_spawn`).
/// Freeing a slot is left to the reader task's exit, so udev `remove` events —
/// whose nodes can no longer be opened — need no handling here.
fn spawn_monitor(ctx: ReaderCtx, dynamic: DynHandles) -> io::Result<tokio::task::JoinHandle<()>> {
    // Filter to the `input` subsystem in-kernel; the resulting socket is
    // non-blocking, so it can be driven from tokio via `AsyncFd`.
    let socket = udev::MonitorBuilder::new()?
        .match_subsystem("input")?
        .listen()?;
    let async_fd = AsyncFd::with_interest(socket, Interest::READABLE)?;
    let mut capture_rx = ctx.capture.clone();

    Ok(tokio::spawn(async move {
        info!("watching for gamepad hotplug events");
        loop {
            tokio::select! {
                changed = capture_rx.changed() => {
                    // A closed capture channel means shutdown; capture on/off
                    // toggles don't concern the monitor (readers handle grabs).
                    if changed.is_err() {
                        break;
                    }
                }
                guard = async_fd.readable() => {
                    let mut guard = match guard {
                        Ok(g) => g,
                        Err(e) => {
                            warn!("udev monitor poll error: {e}");
                            break;
                        }
                    };
                    // Drain every queued event before clearing readiness (the
                    // fd is edge-triggered); `iter` returns once the socket
                    // would block.
                    for event in async_fd.get_ref().iter() {
                        handle_udev_event(&ctx, &event, &dynamic);
                    }
                    guard.clear_ready();
                }
            }
        }
    }))
}

/// React to one udev event: on `add` of a gamepad `/dev/input/event<N>` node
/// (honouring any explicit `-e` filter), open it into a free slot and register
/// its reader task. Non-add events and non-gamepads are ignored.
fn handle_udev_event(ctx: &ReaderCtx, event: &udev::Event, dynamic: &DynHandles) {
    if event.event_type() != udev::EventType::Add {
        return;
    }
    let Some(num) = event
        .sysname()
        .to_str()
        .and_then(|s| s.strip_prefix("event"))
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return;
    };
    if let Some(filter) = &ctx.hotplug_filter
        && !filter.contains(&num)
    {
        return;
    }
    if let Some(t) = open_and_spawn(ctx, num, Scan::Hotplug) {
        dynamic.push(t);
    }
}

/// FIFO mode: create the FIFO if absent, then read raw `input_event` records on
/// a blocking thread, reopening on EOF. Treated as event device #0.
fn spawn_fifo(
    path: &str,
    debug: bool,
    tx: mpsc::Sender<RawEvent>,
) -> io::Result<tokio::task::JoinHandle<()>> {
    let path = PathBuf::from(path);

    match std::fs::metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_fifo() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a FIFO", path.display()),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            make_fifo(&path)?;
            info!("created FIFO {}", path.display());
        }
        Err(e) => return Err(e),
    }

    // Opened read-write and non-blocking, then driven through `AsyncFd` exactly
    // like the udev monitor. Both flags are load-bearing:
    //
    // - **non-blocking** keeps this an ordinary async task. A blocking read on
    //   its own thread cannot be cancelled: `JoinHandle::abort` is a no-op once
    //   a `spawn_blocking` closure is running, so shutdown's abort did nothing
    //   and the runtime then waited for that thread forever — blooter printed
    //   "blooter stopped." and never exited (design/ARCH.md §9).
    // - **read-write** keeps a writer (us) on the FIFO at all times, so an idle
    //   pipe reports "would block" rather than EOF. That removes the reopen loop
    //   the blocking version needed, and with it the blocking `open` — which is
    //   where shutdown actually found this task wedged, waiting for a writer
    //   that never came.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)?;
    let async_fd = AsyncFd::with_interest(file, Interest::READABLE)?;

    // The record layout is native `struct input_event`: timeval(16) + type(2) +
    // code(2) + value(4) = 24 bytes on 64-bit. A writer can split one across
    // reads, so bytes accumulate until a whole record is in hand.
    let task = tokio::spawn(async move {
        const REC: usize = std::mem::size_of::<libc::input_event>();
        let mut buf = [0u8; REC];
        let mut have = 0usize;
        loop {
            let mut guard = match async_fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    warn!("FIFO poll error on {}: {e}", path.display());
                    return;
                }
            };
            // `try_io` clears readiness itself when the read would block.
            let read = guard.try_io(|inner| {
                let mut file: &std::fs::File = inner.get_ref();
                file.read(&mut buf[have..])
            });
            match read {
                Ok(Ok(0)) => guard.clear_ready(),
                Ok(Ok(n)) => {
                    have += n;
                    if have < REC {
                        continue;
                    }
                    have = 0;
                    let type_ = u16::from_ne_bytes([buf[16], buf[17]]);
                    let code = u16::from_ne_bytes([buf[18], buf[19]]);
                    let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
                    let raw = RawEvent {
                        type_,
                        code,
                        value,
                        gamepad: None,
                    };
                    if debug {
                        debug!("[fifo] type={type_} code={code} value={value}");
                    }
                    // Deliver onto the async channel; stop if the receiver is gone.
                    if tx.send(raw).await.is_err() {
                        return;
                    }
                }
                Ok(Err(e)) => {
                    warn!("FIFO read error on {}: {e}", path.display());
                    return;
                }
                Err(_would_block) => continue,
            }
        }
    });

    Ok(task)
}

fn make_fifo(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid FIFO path"))?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Implements `-l`: print the device table and return (design/ARCH.md §8).
pub fn list_devices() {
    println!("List of available input devices:");
    println!("num\tVendor/Product, Name, -x compatible (+/-), * = default scan");
    for num in 0..MAX_EVDEVS {
        let path = event_path(num);
        match Device::open(&path) {
            Ok(mut dev) => {
                let id = dev.input_id();
                let name = dev.name().unwrap_or("").to_string();
                let relevant = if is_relevant(&dev) || is_gamepad(&dev) {
                    "*"
                } else {
                    ""
                };
                let grabbable = match dev.grab() {
                    Ok(()) => {
                        let _ = dev.ungrab();
                        '+'
                    }
                    Err(_) => '-',
                };
                println!(
                    "{:2}\t[{:04x}:{:04x}.{:04x}] '{}' ({}){}",
                    num,
                    id.vendor(),
                    id.product(),
                    id.version(),
                    name,
                    grabbable,
                    relevant
                );
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                println!("{num:2}\t[permission denied]");
            }
            // Device numbering can have gaps; keep scanning to MAX_EVDEVS.
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_pool_claims_lowest_free() {
        let pool = SlotPool::new(3);
        assert_eq!(pool.claim(), Some(0));
        assert_eq!(pool.claim(), Some(1));
        assert_eq!(pool.claim(), Some(2));
        // All three taken.
        assert_eq!(pool.claim(), None);
    }

    #[test]
    fn slot_pool_reuses_freed_slot() {
        let pool = SlotPool::new(2);
        assert_eq!(pool.claim(), Some(0));
        assert_eq!(pool.claim(), Some(1));
        assert_eq!(pool.claim(), None);
        pool.free(0);
        // The freed slot is handed back out.
        assert_eq!(pool.claim(), Some(0));
        assert_eq!(pool.claim(), None);
        // Freeing is idempotent.
        pool.free(1);
        pool.free(1);
        assert_eq!(pool.claim(), Some(1));
    }

    #[test]
    fn slot_pool_zero_slots() {
        let pool = SlotPool::new(0);
        assert_eq!(pool.claim(), None);
    }

    #[test]
    fn slot_pool_concurrent_claims_are_unique() {
        use std::collections::HashSet;
        use std::thread;

        const SLOTS: usize = 32;
        let pool = SlotPool::new(SLOTS);
        let mut handles = Vec::new();
        for _ in 0..SLOTS {
            let pool = pool.clone();
            handles.push(thread::spawn(move || pool.claim()));
        }
        let claimed: Vec<u8> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        // Every slot is handed out exactly once, with no collisions.
        assert_eq!(claimed.len(), SLOTS);
        assert_eq!(claimed.iter().copied().collect::<HashSet<_>>().len(), SLOTS);
        assert_eq!(pool.claim(), None);
    }
}
