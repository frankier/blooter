//! Bluetooth Classic (BR/EDR) transport: raw L2CAP on the two standardized HID
//! PSMs — 0x11 (control) and 0x13 (interrupt). This is blooter's original
//! transport, lifted behind the [`Transport`] seam. See design/ARCH.md §4.

use std::future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluer::l2cap::{SeqPacket, SeqPacketListener, SocketAddr};
use bluer::{Adapter, Address, AddressType};
use log::{debug, error, info, warn};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout_at};

use super::{Accept, DIAL_BACKOFF_MAX, DIAL_BACKOFF_START, Flow, Outbox, Step, Transport, step};
use crate::menu::Pick;
use crate::report::{InputState, RawEvent};
use crate::{AppError, Ctx, Signals};

const CONTROL_PSM: u16 = 0x11;
const INTERRUPT_PSM: u16 = 0x13;
const INTERRUPT_WAIT: Duration = Duration::from_secs(3);
/// HIDP `HID_CONTROL | VIRTUAL_CABLE_UNPLUG`: "forget this device". Sent to make
/// a host drop its bond and its cached SDP record (design/CONNECTION.md §7).
const VIRTUAL_CABLE_UNPLUG: u8 = 0x15;

/// Outcome of the interrupt-channel wait after an inbound control connection.
enum Interrupt {
    /// The interrupt channel connected.
    Got(SeqPacket),
    /// No interrupt channel in time / accept error: go back to accepting.
    Retry,
    /// Terminate the program.
    Shutdown,
}

/// How `wait_connected`'s accept loop finished. Resolved only after the menu
/// task has been cancelled and joined, so the terminal is restored before the
/// function returns (and before `main_loop` logs "host connected").
enum Done {
    Session(SeqPacket, SeqPacket, Address),
    /// The menu asked to fix a host: unplug + unbond it, then keep accepting.
    Fix,
    Shutdown,
}

/// The classic transport: two L2CAP listeners, plus the accepted (or dialed)
/// control and interrupt sockets of the current session (if any). Optionally
/// holds a reconnect target to initiate an outgoing HID connection to
/// (design/CONNECTION.md §3.2) and a receiver for a host picked at runtime by
/// the interactive menu (§6).
pub struct Classic {
    ctrl_listener: SeqPacketListener,
    intr_listener: SeqPacketListener,
    ctrl: Option<SeqPacket>,
    intr: Option<SeqPacket>,
    /// Address of the currently connected host, set alongside `ctrl`/`intr`.
    peer: Option<Address>,
    /// An already-bonded host to initiate an outgoing HID connection to; cleared
    /// once any link is established, so a later drop/loss does not immediately
    /// redial (§3.2). The caller only sets this for a bonded host.
    target: Option<Address>,
    /// The local adapter, when one is available (absent under `-n`). Needed for
    /// the interactive menu and to drop our own bond when a host is unplugged
    /// (§7).
    adapter: Option<Adapter>,
    /// Whether to run the interactive menu. It is (re)spawned at the top of each
    /// `wait_connected` cycle so it re-opens after a disconnect; an incoming
    /// connection preempts it (§6).
    interactive: bool,
    /// Recorded per-host descriptor fingerprints, so hosts holding a stale
    /// cached SDP record can be flagged and fixed (§7).
    hosts: Arc<Mutex<crate::state::Hosts>>,
    /// Fingerprint of the descriptor this run advertises.
    descriptor_fp: u32,
    /// Terminal-ownership coordinator shared with the pairing agent, so an
    /// inbound pairing prompt can borrow the terminal from the running menu
    /// (design/CONNECTION.md §5/§6).
    term_coord: crate::menu::TermCoord,
}

impl Classic {
    /// Bind both HID L2CAP PSMs (control then interrupt). Binding PSMs < 0x1001
    /// needs `CAP_NET_BIND_SERVICE` or root. `target` enables the
    /// reconnect-initiate path (§3.2); `interactive` enables the menu, respawned
    /// each accept cycle (§6).
    pub async fn bind(
        target: Option<Address>,
        adapter: Option<Adapter>,
        interactive: bool,
        hosts: Arc<Mutex<crate::state::Hosts>>,
        descriptor_fp: u32,
        term_coord: crate::menu::TermCoord,
    ) -> Result<Self, AppError> {
        Ok(Self {
            ctrl_listener: bind(CONTROL_PSM).await?,
            intr_listener: bind(INTERRUPT_PSM).await?,
            ctrl: None,
            intr: None,
            peer: None,
            target,
            adapter,
            interactive,
            hosts,
            descriptor_fp,
            term_coord,
        })
    }

    /// Dial an outgoing HID connection to `target` by connecting its control and
    /// interrupt PSMs. The target is already bonded (the caller checks), so the
    /// link is encrypted without initiating a pair here — initiating an outgoing
    /// pair would collide with the host's incoming pair and cancel authentication
    /// (design/CONNECTION.md §3.2).
    async fn dial(&self, target: Address) -> bluer::Result<(SeqPacket, SeqPacket)> {
        let ctrl =
            SeqPacket::connect(SocketAddr::new(target, AddressType::BrEdr, CONTROL_PSM)).await?;
        let intr =
            SeqPacket::connect(SocketAddr::new(target, AddressType::BrEdr, INTERRUPT_PSM)).await?;
        Ok((ctrl, intr))
    }

    /// After an inbound control connection, wait for the matching interrupt
    /// connection within 3 s, still consuming input events and honouring signals.
    async fn await_interrupt(
        &self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
        peer: Address,
    ) -> Interrupt {
        let deadline = Instant::now() + INTERRUPT_WAIT;
        loop {
            tokio::select! {
                r = timeout_at(deadline, self.intr_listener.accept()) => match r {
                    Ok(Ok((intr, _))) => return Interrupt::Got(intr),
                    Ok(Err(e)) => { warn!("interrupt accept failed: {e}"); return Interrupt::Retry; }
                    Err(_) => {
                        error!("timed out waiting for interrupt channel from {peer}");
                        return Interrupt::Retry;
                    }
                },
                Some(ev) = rx.recv() => {
                    if ctx.translate_exits(state, ev) {
                        return Interrupt::Shutdown;
                    }
                }
                _ = signals.term.recv() => return Interrupt::Shutdown,
                _ = signals.hup.recv() => return Interrupt::Shutdown,
                _ = signals.int.recv() => return Interrupt::Shutdown,
            }
        }
    }

    /// Drop our own bond to `addr` and forget its recorded descriptor
    /// fingerprint. Always paired with an unplug: the host drops its bond on
    /// receiving one, and a bond left on only one side makes both reconnect
    /// directions fail (the host cannot authenticate, and our dial is reset).
    async fn unbond(&self, addr: Address) {
        self.hosts.lock().unwrap().forget(addr);
        let Some(adapter) = &self.adapter else {
            warn!("no adapter: remove the bond by hand (bluetoothctl remove {addr})");
            return;
        };
        match adapter.remove_device(addr).await {
            Ok(()) => info!("removed our bond to {addr}"),
            Err(e) => warn!("could not remove our bond to {addr}: {e}"),
        }
    }

    /// "Fix connection" (design/CONNECTION.md §7): make `addr` forget blooter, so
    /// its next pairing re-reads the SDP record instead of replaying a cached
    /// copy of an older HID report descriptor.
    ///
    /// Dials the host's control PSM and sends a virtual-cable unplug, then drops
    /// our own bond. The unplug needs a control channel, so an unreachable host
    /// can only be fixed by hand — reported as such rather than silently.
    async fn fix_host(&mut self, addr: Address) {
        match self.dial(addr).await {
            Ok((ctrl, _intr)) => match ctrl.send(&[VIRTUAL_CABLE_UNPLUG]).await {
                Ok(_) => info!("sent virtual-cable unplug to {addr}"),
                Err(e) => warn!("could not send unplug to {addr}: {e}"),
            },
            Err(e) => warn!(
                "could not reach {addr} to unplug it: {e} \
                 (remove blooter from that host's Bluetooth settings by hand)"
            ),
        }
        self.unbond(addr).await;
        // Both sides are unbonded now, so there is nothing to reconnect to.
        self.target = None;
        println!(
            "Fixed {addr}: it has been asked to forget blooter, and the bond here \
             is gone.\nPair again from that host to pick up the current device layout."
        );
    }
}

impl Transport for Classic {
    async fn send_report(&self, report: &[u8]) -> bool {
        match &self.intr {
            Some(intr) => {
                let res = intr.send(report).await;
                debug!("intr send {:02x?} -> {res:?}", report);
                res.is_ok()
            }
            None => true,
        }
    }

    /// Establish a session, either by accepting an inbound connection (host dials
    /// our control then interrupt PSM within 3 s) or, if a reconnect target is
    /// set, by racing an outgoing HID dial against the inbound accept
    /// (design/CONNECTION.md §3.2). Keeps consuming input events (to track
    /// modifier state and the exit hotkey) and honours signals while waiting.
    async fn wait_connected(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Accept {
        // Dial and menu state live in locals so the select arm bodies can mutate
        // them without conflicting with the shared `this` borrow the concurrent
        // accept/dial futures take (only locals are written inside the loop).
        // `target` may be updated at runtime by a menu pick (§6).
        let mut target = self.target;
        let mut fix: Option<Address> = None;
        let mut next_dial = target.map(|_| Instant::now());
        let mut backoff = DIAL_BACKOFF_START;

        // Spawn this cycle's interactive menu (interactive mode only); it is a
        // local so no `self` field is borrowed inside the select loop (see the
        // note above). Hosts bonded under a different descriptor are flagged in
        // the list and fixable with `[f]` (§7).
        let stale = self.hosts.lock().unwrap().stale(self.descriptor_fp);
        let mut menu = crate::menu::Session::spawn(
            self.adapter.as_ref(),
            self.interactive,
            crate::menu::Kind::Classic,
            stale,
            // Classic builds its list by scanning, so it needs no remembered
            // hosts (that union is the BLE menu's, §6).
            Vec::new(),
            // ...and no muted host: dropping a Classic session drops the link
            // rather than muting it (§6.2).
            None,
            &self.term_coord,
        );

        let done = loop {
            // Shared borrow for the concurrent accept/dial futures; per-iteration
            // copies so those futures do not borrow the locals the arms reassign.
            let this: &Classic = self;
            let due = next_dial;
            let dial_target = target;
            let dial = async {
                match (due, dial_target) {
                    (Some(at), Some(t)) => {
                        sleep_until(at).await;
                        Some(this.dial(t).await)
                    }
                    _ => future::pending().await,
                }
            };

            tokio::select! {
                // Inbound: control channel, then the matching interrupt channel.
                r = this.ctrl_listener.accept() => match r {
                    Ok((ctrl, sa)) => {
                        match self.await_interrupt(rx, state, ctx, signals, sa.addr).await {
                            Interrupt::Got(intr) => {
                                if menu.is_open() {
                                    info!("incoming connection from {}; using it and \
                                           closing the menu", sa.addr);
                                }
                                break Done::Session(ctrl, intr, sa.addr);
                            }
                            Interrupt::Retry => {} // fall through, keep waiting/dialing
                            Interrupt::Shutdown => break Done::Shutdown,
                        }
                    }
                    Err(e) => warn!("control accept failed: {e}"),
                },
                // Outbound: reconnect-initiate dial.
                Some(outcome) = dial => match outcome {
                    Ok((ctrl, intr)) => {
                        let peer = target.expect("dialed with a target");
                        info!("reconnect-initiate to {peer} succeeded");
                        break Done::Session(ctrl, intr, peer);
                    }
                    Err(e) => {
                        warn!("reconnect-initiate dial failed: {e}");
                        next_dial = Some(Instant::now() + backoff);
                        backoff = (backoff * 2).min(DIAL_BACKOFF_MAX);
                    }
                },
                // Menu pick: start dialing the chosen host (still racing inbound).
                picked = menu.recv() => {
                    match picked {
                        // A fix tears the bond down rather than connecting, so it
                        // must not become a dial target (§7).
                        Some(Pick::Fix(addr)) => {
                            info!("menu selected {addr}; fixing connection");
                            fix = Some(addr);
                            target = None;
                            next_dial = None;
                        }
                        Some(Pick::Connect(addr)) => {
                            info!("menu selected {addr}; initiating HID connection");
                            target = Some(addr);
                            next_dial = Some(Instant::now());
                            backoff = DIAL_BACKOFF_START;
                        }
                        // `[u]` is a BLE-only repair: on Classic the unplug in
                        // `fix_host` already drops the bond on both sides (§7.2a).
                        Some(Pick::Forget(addr)) => {
                            warn!("ignoring a forget pick for {addr}: Classic has no [u]");
                        }
                        // Nothing to resume: a dropped Classic session dropped
                        // the link with it, so the host is simply gone (§6.2).
                        Some(Pick::Resume(addr)) => {
                            warn!("ignoring a resume pick for {addr}: Classic has no muted state");
                        }
                        None => {}
                    }
                    // A fix needs `&mut self`, which the accept/dial futures
                    // borrow; leave the loop and perform it after they are gone.
                    if fix.is_some() {
                        break Done::Fix;
                    }
                }
                Some(ev) = rx.recv() => {
                    if ctx.translate_exits(state, ev) {
                        break Done::Shutdown;
                    }
                }
                _ = signals.term.recv() => break Done::Shutdown,
                _ = signals.hup.recv() => break Done::Shutdown,
                _ = signals.int.recv() => break Done::Shutdown, // no session active
            }
        };

        // Preempt this cycle's menu and wait for it to restore the terminal
        // before returning — ordered ahead of any further output (§6).
        menu.finish().await;

        match done {
            Done::Shutdown => Accept::Shutdown,
            // Perform the fix now the menu task is joined and the accept/dial
            // futures no longer borrow `self`, then go back to accepting.
            Done::Fix => {
                if let Some(addr) = fix {
                    self.fix_host(addr).await;
                }
                Box::pin(self.wait_connected(rx, state, ctx, signals)).await
            }
            Done::Session(ctrl_sock, intr_sock, peer) => {
                self.ctrl = Some(ctrl_sock);
                self.intr = Some(intr_sock);
                self.peer = Some(peer);
                // Remember which descriptor — and which transport — this host is
                // now bonded under, so a later change to either can be detected
                // (§7, §8.2).
                self.hosts.lock().unwrap().set(
                    peer,
                    self.descriptor_fp,
                    crate::config::Protocol::Classic,
                );
                // A link is up (either direction): stop initiating so an
                // intentional drop or a link loss does not immediately redial
                // (design/CONNECTION.md §3.2). The host can dial back, or restart
                // to re-initiate.
                self.target = None;
                Accept::Connected(peer.to_string())
            }
        }
    }

    /// Forward translated reports on the interrupt channel, service the control
    /// channel minimally, and watch for disconnect, hotkeys, and signals. SIGINT
    /// is ignored while connected (design/ARCH.md §9).
    async fn run_session(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Flow {
        let ctrl = self.ctrl.as_ref().expect("session ctrl socket");
        let intr = self.intr.as_ref().expect("session intr socket");
        let mut ctrl_buf = [0u8; 256];
        let mut intr_buf = [0u8; 256];
        // Set when the host unplugs us: it drops its bond on doing so, so ours
        // has to go too or neither side can reconnect (§7).
        let mut unplugged = false;
        debug!("session started; forwarding reports on the interrupt channel");

        // Allocated once for the connection (design/ARCH.md §7.2c).
        let mut out = Outbox::new(ctx.buffer, ctx.batch, self.flush_interval(), ctx.overflow);

        let flow = loop {
            tokio::select! {
                inc = out.next(rx) => {
                    if let Step::Return(f) = step(self, ctx, state, &mut out, inc).await {
                        break f;
                    }
                }
                r = ctrl.recv(&mut ctrl_buf) => match r {
                    Ok(0) => { debug!("control channel EOF"); break Flow::Continue; }
                    Ok(n) => {
                        debug!("ctrl recv {n} bytes: {:02x?}", &ctrl_buf[..n]);
                        if handle_control(ctrl, &ctrl_buf[..n]).await {
                            unplugged = true;
                            break Flow::Continue; // VIRTUAL_CABLE_UNPLUG
                        }
                    }
                    Err(e) => { warn!("control channel error: {e}"); break Flow::Continue; }
                },
                r = intr.recv(&mut intr_buf) => match r {
                    Ok(0) => { debug!("interrupt channel EOF"); break Flow::Continue; }
                    Ok(n) => {
                        debug!("intr recv {n} bytes: {:02x?}", &intr_buf[..n]);
                        // A VIRTUAL_CABLE_UNPLUG may also arrive here.
                        if n > 0 && intr_buf[0] == VIRTUAL_CABLE_UNPLUG {
                            unplugged = true;
                            break Flow::Continue;
                        }
                    }
                    Err(e) => { warn!("interrupt channel error: {e}"); break Flow::Continue; }
                },
                _ = signals.term.recv() => break Flow::Shutdown,
                _ = signals.hup.recv() => break Flow::Shutdown,
                // SIGINT deliberately not selected: ignored during a session.
            }
        };

        // Drop the session sockets so the next accept starts fresh.
        self.ctrl = None;
        self.intr = None;
        let peer = self.peer.take();
        if unplugged && let Some(addr) = peer {
            info!("{addr} unplugged us (virtual cable); dropping our bond to match");
            // The line above is an account of what blooter did; this one is the
            // thing the user has to be told (design/CONNECTION.md §8.2). An
            // unplug is the one place a host's decision to forget blooter
            // actually reaches us, so it is said plainly rather than left to be
            // inferred from a connection that silently stops working.
            warn!(
                "{addr} has removed blooter — it no longer knows us, so it will not connect. \
                 Pair again from that host's Bluetooth settings."
            );
            self.unbond(addr).await;
        }
        flow
    }
}

async fn bind(psm: u16) -> Result<SeqPacketListener, AppError> {
    let sa = SocketAddr::new(Address::any(), AddressType::BrEdr, psm);
    SeqPacketListener::bind(sa).await.map_err(|e| {
        AppError::new(
            3,
            format!(
                "cannot bind L2CAP PSM 0x{psm:02x}: {e} \
                 (need CAP_NET_BIND_SERVICE / root, and bluetoothd -P input)"
            ),
        )
    })
}

/// Handle one HIDP control-channel message. Returns `true` if it signals a
/// disconnect (VIRTUAL_CABLE_UNPLUG). Responds `HANDSHAKE
/// ERR_UNSUPPORTED_REQUEST` (0x03) to transfer requests (design/ARCH.md §4).
async fn handle_control(ctrl: &SeqPacket, data: &[u8]) -> bool {
    let Some(&header) = data.first() else {
        return false;
    };
    if header == VIRTUAL_CABLE_UNPLUG {
        return true; // HID_CONTROL / VIRTUAL_CABLE_UNPLUG
    }
    // Transaction type is the high nibble: HANDSHAKE=0, HID_CONTROL=1, DATA=0xA.
    // Reject GET_REPORT/SET_REPORT/GET_PROTOCOL/SET_PROTOCOL etc. as unsupported.
    let ttype = header >> 4;
    if ttype != 0x0 && ttype != 0x1 && ttype != 0xA {
        debug!("ctrl: replying ERR_UNSUPPORTED_REQUEST to header 0x{header:02x}");
        let _ = ctrl.send(&[0x03]).await;
    }
    false
}
