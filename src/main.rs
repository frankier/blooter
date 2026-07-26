//! blooter — a Bluetooth HID *device* emulator (keyboard, mouse and gamepads)
//! built on Rust/BlueR. Presents over Bluetooth Classic (BR/EDR HID) by default
//! or Bluetooth Low Energy (HID-over-GATT) via `[connection] protocol = "ble"`.
//! See design/ARCH.md.

mod agent;
mod cli;
mod config;
mod input;
mod keymap;
mod menu;
mod report;
mod sdp;
mod setup;
mod state;
mod transport;

use std::cell::Cell;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use bluer::rfcomm::{Profile, ReqError, Role};
use bluer::{Session, Uuid};
use futures::StreamExt;
use log::{error, info, warn};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, watch};

use config::Hotkeys;
use report::{InputState, Outcome, RawEvent, translate};
use transport::{Accept, AnyTransport, Classic, Flow, Le, Transport};

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        cli::ParseResult::Run(a) => a,
        cli::ParseResult::Help => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        cli::ParseResult::Error(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };

    let level = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    // `-l` needs no async runtime or Bluetooth.
    if args.list {
        input::list_devices();
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start async runtime: {e}");
            return ExitCode::from(2);
        }
    };

    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code)
        }
    }
}

/// A startup/runtime error carrying the desired process exit code.
pub struct AppError {
    msg: String,
    pub code: u8,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl AppError {
    pub fn new(code: u8, msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            code,
        }
    }
}

pub struct Signals {
    pub int: Signal,
    pub term: Signal,
    pub hup: Signal,
}

async fn run(args: cli::Args) -> Result<(), AppError> {
    // --- Configuration (hotkey chords, gamepad slots) ---
    let cfg = match &args.config {
        Some(p) => config::load(Path::new(p)).map_err(|e| AppError::new(1, e))?,
        None => match config::default_path() {
            Some(p) => {
                info!("using config {}", p.display());
                config::load(&p).map_err(|e| AppError::new(1, e))?
            }
            None => config::Config::default(),
        },
    };
    let hotkeys = cfg.hotkeys;

    // How many gamepad controllers to advertise (report IDs 3, 4, …). FIFO mode
    // has no evdev gamepads, so none are advertised there.
    let n_gamepads = if args.fifo.is_some() {
        0
    } else {
        match cfg.gamepad_slots {
            config::GamepadSlots::Fixed(n) => n,
            config::GamepadSlots::Initial => input::count_gamepads(&args.event_devices),
        }
    };
    if n_gamepads > 0 {
        info!("advertising {n_gamepads} gamepad controller(s)");
    }

    // Whether to open gamepads plugged in at runtime into free slots. FIFO mode
    // has no evdev gamepads; `enabled` gates auto on a fixed, non-zero count.
    let hotplug = args.fifo.is_none() && n_gamepads > 0 && cfg.hotplug.enabled(cfg.gamepad_slots);
    if hotplug {
        info!("gamepad hotplug enabled");
    }

    // Hosts cache blooter's SDP record (HID report descriptor included) for the
    // lifetime of their bond, so changing the descriptor — which is exactly what
    // changing the gamepad slot count does — has no effect on an already-bonded
    // host until it is re-paired (design/CONNECTION.md §7).
    let descriptor_fp = sdp::descriptor_fingerprint(n_gamepads);
    let hosts = std::sync::Arc::new(std::sync::Mutex::new(state::Hosts::load()));
    let stale = hosts.lock().unwrap().stale(descriptor_fp);
    if !stale.is_empty() {
        let list: Vec<String> = stale.iter().map(|a| a.to_string()).collect();
        warn!(
            "the HID device layout changed since these hosts were paired: {}. \
             They are still using the layout they cached, so any gamepad change \
             will not reach them; pick one in the menu and press [f] to fix it.",
            list.join(", ")
        );
    }

    // --- Bluetooth session ---
    let session = Session::new()
        .await
        .map_err(|e| AppError::new(1, format!("cannot connect to bluetoothd: {e}")))?;

    // --- Input sources (design/ARCH.md §6) ---
    // The watch channel carries the input-capture state to the device readers
    // (they release/reacquire the -x exclusive grab on changes).
    // Starts `false`: no device is grabbed until a host session is established,
    // so the interactive host menu below (and the local session while idle) keep
    // the keyboard. The grab is switched on per session in the accept loop.
    let (capture_tx, capture_rx) = watch::channel(false);
    let (tx, mut rx) = mpsc::channel::<RawEvent>(256);
    let inputs = input::spawn(
        &args.event_devices,
        args.fifo.as_deref(),
        args.grab,
        args.debug,
        n_gamepads,
        hotplug,
        capture_rx,
        tx,
    )
    .map_err(|e| AppError::new(2, format!("cannot open input: {e}")))?;

    // --- Signal handlers (design/ARCH.md §9) ---
    let mut signals = Signals {
        int: signal(SignalKind::interrupt())
            .map_err(|e| AppError::new(1, format!("signal setup failed: {e}")))?,
        term: signal(SignalKind::terminate())
            .map_err(|e| AppError::new(1, format!("signal setup failed: {e}")))?,
        hup: signal(SignalKind::hangup())
            .map_err(|e| AppError::new(1, format!("signal setup failed: {e}")))?,
    };

    let mut state = InputState::with_gamepads(n_gamepads);
    let ctx = Ctx {
        hotkeys: &hotkeys,
        capture_tx: &capture_tx,
        connected: Cell::new(false),
    };

    // --- Shared pairing agent (design/CONNECTION.md §5) ---
    // One agent serves both transports (HID bonding needs one; without it a
    // Classic incoming pair stalls). Its mode is the config value, or inferred
    // from the TTY. The handle is kept alive for the program's lifetime.
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let pairing_mode = agent::resolve_mode(cfg.pairing, interactive);
    // Shared terminal-ownership coordinator: lets a confirm-mode pairing prompt
    // suspend the interactive menu (which owns the terminal in raw mode) before
    // reading the reply on the TTY (design/CONNECTION.md §5/§6). A no-op when no
    // menu is running (LE, non-interactive, or after the menu resolves).
    let term_coord = menu::TermCoord::default();
    let _agent = session
        .register_agent(agent::agent(pairing_mode, term_coord.clone()))
        .await
        .map_err(|e| AppError::new(1, format!("cannot register pairing agent: {e}")))?;

    // A configured reconnect target applies to either transport's resolution.
    let configured_target = cfg.reconnect.as_deref().and_then(agent::parse_address);

    // --- Transport-specific Bluetooth registration ---
    // Classic keeps a profile-registration task and an adapter-setup guard alive
    // for the program's lifetime; LE keeps them inside the transport instead.
    let mut profile_task = None;
    let mut _bt = None;
    // If we turn the adapter discoverable, remember it so shutdown can restore
    // the previous state (design/CONNECTION.md §6).
    let mut discoverable_reset: Option<bluer::Adapter> = None;
    let transport = match cfg.protocol {
        config::Protocol::Classic => {
            profile_task = register_profile(&session, &args, n_gamepads).await?;
            let adapter = if args.nosetup {
                None
            } else {
                match session.default_adapter().await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        warn!("no default adapter, skipping setup: {e}");
                        None
                    }
                }
            };
            _bt = adapter.as_ref().and_then(|a| {
                let index = a
                    .name()
                    .strip_prefix("hci")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                match setup::apply(index) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        warn!("adapter setup failed: {e} (needs CAP_NET_ADMIN; -n disables)");
                        None
                    }
                }
            });
            if let Some(a) = &adapter {
                let _ = a.set_powered(true).await;
                let _ = a.set_pairable(true).await;
                // Advertise ourselves so a host can find and connect to us. Save
                // the prior state for restoration on exit.
                let was_discoverable = a.is_discoverable().await.unwrap_or(false);
                let _ = a.set_discoverable_timeout(0).await;
                if a.set_discoverable(true).await.is_ok() {
                    println!(
                        "blooter is now discoverable as \"blooter\" — other machines \
                         can find and connect to it."
                    );
                    if !was_discoverable {
                        discoverable_reset = Some(a.clone());
                    }
                }
            }
            // Resolve any configured reconnect target (kept only if already
            // bonded). In interactive mode the transport (re)spawns the menu each
            // accept cycle; its pick runs concurrently with the accept loop and
            // an incoming connection can preempt it (design/CONNECTION.md §3.2, §6).
            let target = initiate_target(adapter.as_ref(), configured_target).await;
            AnyTransport::Classic(
                Classic::bind(
                    target,
                    adapter.clone(),
                    interactive,
                    hosts.clone(),
                    descriptor_fp,
                    term_coord.clone(),
                )
                .await?,
            )
        }
        config::Protocol::Ble => {
            let adapter = session
                .default_adapter()
                .await
                .map_err(|e| AppError::new(1, format!("no default adapter for LE: {e}")))?;
            let _ = adapter.set_pairable(true).await;
            AnyTransport::Le(Le::new(adapter, n_gamepads).await?)
        }
    };

    println!("The HID-Client is now ready to accept connections from another machine");

    main_loop(transport, &mut rx, &mut state, &ctx, &mut signals).await;

    // --- Clean shutdown (design/ARCH.md §9) ---
    if let Some(t) = profile_task {
        t.abort(); // dropping the handle unregisters the profile
    }
    // Closing the capture channel tells each reader to release its grab
    // (injecting the touchpad reset) and exit; wait for that before the fds
    // close, so libinput resumes from a clean state.
    drop(capture_tx);
    inputs.shutdown().await;
    // Restore the adapter's prior discoverable state if we changed it (§6).
    if let Some(a) = &discoverable_reset {
        let _ = a.set_discoverable(false).await;
    }
    flush_stdin();
    println!("blooter stopped.");
    Ok(())
    // `_bt` drops here, restoring the adapter class/name/SSP.
}

/// The accept → session loop, driven over whichever transport was selected.
async fn main_loop(
    mut transport: AnyTransport,
    rx: &mut mpsc::Receiver<RawEvent>,
    state: &mut InputState,
    ctx: &Ctx<'_>,
    signals: &mut Signals,
) {
    loop {
        match transport.wait_connected(rx, state, ctx, signals).await {
            Accept::Shutdown => break,
            Accept::Connected(peer) => {
                info!("host connected: {peer}");
                // Reset per-session state and drain any stale pending events so
                // they are not delivered to the newly connected host. The grab
                // is taken now (connection made, input is forwarded) and released
                // again below when the session ends.
                state.reset();
                ctx.connected.set(true);
                ctx.capture_tx.send_replace(true);
                while rx.try_recv().is_ok() {}
                transport.on_connected(state).await;

                let flow = transport.run_session(rx, state, ctx, signals).await;
                if matches!(flow, Flow::Shutdown) {
                    break;
                }
                info!("host disconnected");
                // Release the exclusive grab while idle so the local session gets
                // the keyboard back until the next host connects.
                ctx.connected.set(false);
                ctx.capture_tx.send_replace(false);
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Register the classic HID profile with `bluetoothd` (unless `-s`). The
/// returned task owns the profile handle for the program's lifetime and
/// rejects/ignores any inbound connection (real traffic runs on our own L2CAP
/// listeners). design/ARCH.md §3.
async fn register_profile(
    session: &Session,
    args: &cli::Args,
    n_gamepads: usize,
) -> Result<Option<tokio::task::JoinHandle<()>>, AppError> {
    if args.skipsdp {
        return Ok(None);
    }
    let uuid: Uuid = sdp::HID_UUID.parse().expect("valid HID UUID");
    let profile = Profile {
        uuid,
        name: Some("blooter HID".to_string()),
        role: Some(Role::Server),
        require_authentication: Some(false),
        require_authorization: Some(false),
        service_record: Some(sdp::service_record_xml(n_gamepads)),
        ..Default::default()
    };
    let mut handle = session.register_profile(profile).await.map_err(|e| {
        AppError::new(
            1,
            format!(
                "profile registration failed: {e}\n\
                 \n\
                 note: the HID profile UUID could not be claimed. Common causes:\n\
                 \x20 - another blooter instance is already running (only one may\n\
                 \x20   register the HID profile at a time); or\n\
                 \x20 - bluetoothd was started with its built-in `input` plugin\n\
                 \x20   enabled, which already owns the HID UUID. Start it with\n\
                 \x20   `bluetoothd -P input`. Under systemd:\n\
                 \x20     sudo systemctl edit bluetooth\n\
                 \x20   add, matching the path in your distro's original ExecStart:\n\
                 \x20     [Service]\n\
                 \x20     ExecStart=\n\
                 \x20     ExecStart=/path/to/bluetoothd -P input\n\
                 \x20   then: sudo systemctl restart bluetooth"
            ),
        )
    })?;
    Ok(Some(tokio::spawn(async move {
        while let Some(req) = handle.next().await {
            info!("ignoring inbound profile connection from {}", req.device());
            req.reject(ReqError::Rejected);
        }
    })))
}

/// Keep a reconnect target only if it is already bonded (design/CONNECTION.md
/// §3.2, §6). blooter reconnects to hosts it already knows but never initiates
/// pairing itself, so an unbonded pick is dropped (with a hint) and blooter
/// simply accepts that host's incoming connection instead.
async fn initiate_target(
    adapter: Option<&bluer::Adapter>,
    target: Option<bluer::Address>,
) -> Option<bluer::Address> {
    let addr = target?;
    // No adapter to check against (e.g. `-n`): trust the configured value.
    let Some(adapter) = adapter else {
        return Some(addr);
    };
    let bonded = match adapter.device(addr) {
        Ok(dev) => dev.is_paired().await.unwrap_or(false),
        Err(_) => false,
    };
    if bonded {
        Some(addr)
    } else {
        warn!(
            "host {addr} is not bonded; will only accept its incoming connection \
             (pair it from that host to enable reconnect-initiate)"
        );
        None
    }
}

/// Translation context: the configured hotkeys plus the capture watch sender
/// that drives grab/ungrab in the input reader tasks. `connected` tracks whether
/// a host session is active, so the exclusive grab is only ever taken while a
/// connection exists (keystrokes are being forwarded somewhere).
pub struct Ctx<'a> {
    hotkeys: &'a Hotkeys,
    pub capture_tx: &'a watch::Sender<bool>,
    pub connected: Cell<bool>,
}

impl Ctx<'_> {
    /// Translate one event and apply capture-state side effects. Capture-hotkey
    /// changes only drive the grab while a session is connected; toggling
    /// capture while disconnected must not grab the keyboard.
    pub fn translate(&self, state: &mut InputState, ev: RawEvent) -> Outcome {
        let out = translate(self.hotkeys, state, ev);
        if self.connected.get() {
            match out {
                Outcome::CaptureOn => {
                    info!("input capture enabled");
                    self.capture_tx.send_replace(true);
                }
                Outcome::CaptureOff => {
                    info!("input capture disabled");
                    self.capture_tx.send_replace(false);
                }
                _ => {}
            }
        }
        out
    }
}

/// Drain any pending bytes from stdin if it is a TTY, so keystrokes typed while
/// forwarding do not spill into the terminal after exit (design/ARCH.md §9).
fn flush_stdin() {
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
    }
}
