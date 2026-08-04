//! Transport seam: the input→report core (report.rs, keymap.rs, input.rs) is
//! shared; only *delivery* and *discovery* differ between Bluetooth Classic
//! (BR/EDR HID) and Bluetooth Low Energy (HID-over-GATT). See design/ARCH.md §4.
//!
//! A [`Transport`] owns the connection lifecycle (accept a host, run a session)
//! and knows how to push one already-built HID input report. The main loop
//! (`main::main_loop`) drives whichever transport was selected on the command
//! line through the same event → report loop.

pub mod classic;
pub mod le;

use crate::config::{Batch, Overflow};
use crate::report::{InputState, Outcome, Outcomes, RawEvent, Report};
use crate::{Ctx, Signals};
use std::future;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

pub use classic::Classic;
pub use le::Le;

/// Backoff between failed attempts to initiate a link to a known host — the
/// Classic HID dial and the LE connect alike (design/CONNECTION.md §3.2, §4).
const DIAL_BACKOFF_START: Duration = Duration::from_secs(1);
const DIAL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Outcome of the accept phase.
pub enum Accept {
    /// A host is connected (classic) or subscribed (LE); the string identifies
    /// the peer for logging.
    Connected(String),
    /// Terminate the program.
    Shutdown,
}

/// Outcome of the connected phase.
pub enum Flow {
    /// Session ended; go back to accepting.
    Continue,
    /// Terminate the program.
    Shutdown,
}

/// One step of the per-event dispatch (see [`dispatch`]).
pub enum Step {
    /// Keep running the session loop.
    Continue,
    /// Leave the session loop with this flow.
    Return(Flow),
}

/// What a session loop woke up for (see [`Outbox::next`]).
pub enum Incoming {
    /// An input event to translate.
    Event(RawEvent),
    /// The batching interval elapsed with reports waiting.
    FlushDue,
    /// The input channel closed: shutdown.
    Closed,
}

/// The outgoing report buffer for one connection: a fixed-capacity ring of
/// already-built reports, drained by a flush (design/ARCH.md §7.2c).
///
/// It exists because the session loop sends serially — `send_report().await`
/// blocks the loop — so on a link slower than the pointer, motion queues up and
/// the pointer keeps gliding after the user stops. Consecutive mouse reports
/// merge into the ring's tail instead of queueing, which bounds the lag to one
/// flush interval no matter how fast events arrive.
///
/// Allocated once per connection; pushing is a copy into a slot, never an
/// allocation and never a spawned task.
pub struct Outbox {
    q: Box<[Report]>,
    head: usize,
    len: usize,
    /// Set at a frame boundary when the ring holds anything: a `SYN_REPORT` is
    /// necessary for a flush in every mode, so a timer that fires mid-frame
    /// waits for this rather than splitting the frame.
    armed: bool,
    interval: Option<Duration>,
    overflow: Overflow,
    last_flush: Instant,
}

impl Outbox {
    /// A buffer holding `capacity` reports, batching per `batch` with `auto` as
    /// the transport's own `Batch::Auto` spacing.
    pub fn new(capacity: usize, batch: Batch, auto: Duration, overflow: Overflow) -> Self {
        Outbox {
            q: vec![Report::new(&[]); capacity.max(1)].into_boxed_slice(),
            head: 0,
            len: 0,
            armed: false,
            interval: batch.interval(auto),
            overflow,
            last_flush: Instant::now(),
        }
    }

    fn tail(&mut self) -> Option<&mut Report> {
        if self.len == 0 {
            return None;
        }
        let idx = (self.head + self.len - 1) % self.q.len();
        Some(&mut self.q[idx])
    }

    fn is_full(&self) -> bool {
        self.len == self.q.len()
    }

    /// Queue a report, merging it into the tail when that is safe.
    ///
    /// Merging is only ever backwards into the immediately preceding entry, and
    /// only when that entry is a mouse report carrying the identical button
    /// byte. A keyboard or gamepad tail blocks the merge, as does a button
    /// change — so nothing can overtake anything else and no button transition
    /// is lost. Returns `false` if the ring was full.
    fn push(&mut self, r: Report) -> bool {
        if self.merge_tail(&r) {
            return true;
        }
        if self.is_full() {
            return false;
        }
        let idx = (self.head + self.len) % self.q.len();
        self.q[idx] = r;
        self.len += 1;
        true
    }

    /// Try to fold `r` into the tail entry. Only mouse reports (id 1) with the
    /// same button byte and the same length merge, and only when no axis
    /// saturates — a saturating sum is pushed separately so no motion is lost.
    fn merge_tail(&mut self, r: &Report) -> bool {
        if r.id() != 1 {
            return false;
        }
        let Some(tail) = self.tail() else {
            return false;
        };
        let (t, s) = (tail.as_slice(), r.as_slice());
        if t.len() != s.len() || t[1] != 1 || t[2] != s[2] {
            return false;
        }
        let merged = match t.len() {
            6 => {
                let sum = |a: u8, b: u8| i32::from(a as i8) + i32::from(b as i8);
                let (x, y, w) = (sum(t[3], s[3]), sum(t[4], s[4]), sum(t[5], s[5]));
                if !(-127..=127).contains(&x)
                    || !(-127..=127).contains(&y)
                    || !(-127..=127).contains(&w)
                {
                    return false;
                }
                Report::new(&[
                    0xA1,
                    0x01,
                    t[2],
                    x as i8 as u8,
                    y as i8 as u8,
                    w as i8 as u8,
                ])
            }
            8 => {
                let axis = |b: &[u8]| i32::from(i16::from_le_bytes([b[0], b[1]]));
                let x = axis(&t[3..5]) + axis(&s[3..5]);
                let y = axis(&t[5..7]) + axis(&s[5..7]);
                let w = i32::from(t[7] as i8) + i32::from(s[7] as i8);
                if !(-32767..=32767).contains(&x)
                    || !(-32767..=32767).contains(&y)
                    || !(-127..=127).contains(&w)
                {
                    return false;
                }
                let [xl, xh] = (x as i16).to_le_bytes();
                let [yl, yh] = (y as i16).to_le_bytes();
                Report::new(&[0xA1, 0x01, t[2], xl, xh, yl, yh, w as i8 as u8])
            }
            _ => return false,
        };
        *tail = merged;
        true
    }

    /// Drain pending pointer state into the ring at a frame boundary, applying
    /// the `[pointer] overflow` policy when it exceeds one report's range.
    fn drain_frame(&mut self, state: &mut InputState) {
        match self.overflow {
            // Emit as many reports as it takes: lossless, bounded by the ring.
            Overflow::Burst => {
                while let Some(r) = state.take_mouse_frame() {
                    if !self.push(r) {
                        break;
                    }
                }
            }
            // One saturated report; the remainder rides along next frame.
            Overflow::Carry => {
                if let Some(r) = state.take_mouse_frame() {
                    self.push(r);
                }
            }
            // One saturated report; the remainder is dropped.
            Overflow::Clamp => {
                if let Some(r) = state.take_mouse_frame() {
                    self.push(r);
                }
                state.clear_mouse();
            }
        }
    }

    /// Discard everything queued, for the paths that release host-side state
    /// directly and must not be followed by stale reports (§6.3).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.armed = false;
    }

    /// Send everything queued, oldest first. Returns `false` if the peer is
    /// gone and the session should end.
    async fn flush<T: Transport + ?Sized>(&mut self, t: &T) -> bool {
        while self.len > 0 {
            let r = self.q[self.head];
            self.head = (self.head + 1) % self.q.len();
            self.len -= 1;
            if !t.send_report(r.as_slice()).await {
                self.clear();
                return false;
            }
        }
        self.armed = false;
        self.last_flush = Instant::now();
        true
    }

    /// The deadline a `FlushDue` wakeup is owed, if any.
    fn deadline(&self) -> Option<Instant> {
        match self.interval {
            Some(iv) if self.armed => Some(self.last_flush + iv),
            _ => None,
        }
    }

    /// Wait for the next input event, or for a due flush.
    ///
    /// Cancel-safe: `rx.recv()` and `sleep_until` both are, and nothing is
    /// mutated before one of them resolves. With no deadline armed the timer
    /// branch is `future::pending()`, so an idle session runs no timer at all.
    pub async fn next(&mut self, rx: &mut mpsc::Receiver<RawEvent>) -> Incoming {
        let deadline = self.deadline();
        let timer = async {
            match deadline {
                Some(at) => sleep_until(at).await,
                None => future::pending().await,
            }
        };
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => Incoming::Event(ev),
                None => Incoming::Closed,
            },
            _ = timer => Incoming::FlushDue,
        }
    }
}

/// A Bluetooth transport that can accept a host and push HID input reports.
///
/// Reports are passed as the full wire bytes built by `report.rs`
/// (`[0xA1, report_id, payload…]`). The classic transport writes them verbatim
/// on the interrupt channel; the LE transport strips the `0xA1` HIDP header and
/// report id and notifies the matching Report characteristic (design/ARCH.md §4.2).
pub trait Transport {
    /// Wait for a host to connect / subscribe. While waiting, keep consuming
    /// input events (to track modifier state and honour the exit hotkey) and
    /// honour termination signals.
    async fn wait_connected(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Accept;

    /// Run the connected session: forward translated reports, watch for
    /// disconnect, hotkeys and signals. Returns whether to continue accepting
    /// or shut down.
    async fn run_session(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Flow;

    /// Push one already-built HID input report (`[0xA1, report_id, payload…]`).
    /// Returns `false` if the peer is gone and the session should end. A
    /// transport with no subscriber for the report id may no-op and return
    /// `true` (design/ARCH.md §4.2).
    async fn send_report(&self, report: &[u8]) -> bool;

    /// Called once immediately after a host connects, before the session loop.
    /// LE uses it to push initial zeroed reports so the host has state; classic
    /// does nothing.
    async fn on_connected(&self, _state: &InputState) {}

    /// Called when the `drop_connection` hotkey ends a session, before the loop
    /// returns to accepting. Classic needs nothing — returning from
    /// `run_session` drops its L2CAP sockets, which *is* the disconnect — while
    /// LE mutes the still-live link instead (design/CONNECTION.md §6.2).
    async fn drop_session(&self) {}

    /// Release all inputs host-side: an all-keys-up keyboard report, a neutral
    /// report for every advertised gamepad and — with `[remote]` on — a
    /// nothing-held consumer report, so nothing stays latched when a session is
    /// dropped, blooter exits, or capture is paused (design/REMOTE.md §7).
    async fn release_all(&self, state: &InputState) {
        self.send_report(InputState::keys_up_report().as_slice())
            .await;
        for r in state.gamepad_neutral_reports() {
            self.send_report(r.as_slice()).await;
        }
        if let Some(r) = state.consumer_up_report() {
            self.send_report(r.as_slice()).await;
        }
    }

    /// The `Batch::Auto` spacing between pointer flushes for this transport
    /// (design/ARCH.md §7.2c).
    fn flush_interval(&self) -> Duration {
        Duration::from_millis(8)
    }
}

/// Act on one session-loop wakeup, shared by every transport. Queues report
/// outcomes in `out`, flushes it when the batching policy says so, and maps
/// hotkey outcomes (drop/exit/capture) onto the session control flow.
///
/// The paths that release host-side state — drop, exit, capture-off — clear the
/// ring *before* [`Transport::release_all`], which sends directly: a queued
/// keyboard report landing after the all-keys-up report would re-latch a key on
/// the host.
pub async fn step<T: Transport>(
    t: &T,
    ctx: &Ctx<'_>,
    state: &mut InputState,
    out: &mut Outbox,
    inc: Incoming,
) -> Step {
    let ev = match inc {
        Incoming::Closed => return Step::Return(Flow::Shutdown),
        Incoming::FlushDue => {
            return if out.flush(t).await {
                Step::Continue
            } else {
                Step::Return(Flow::Continue)
            };
        }
        Incoming::Event(ev) => ev,
    };
    let mut outs = Outcomes::default();
    ctx.translate(state, ev, &mut outs);
    // One event can yield several reports: a chord that turns out not to be
    // coming replays the keys it held back, in press order (design/ARCH.md §7.3).
    for i in 0..outs.len() {
        match outs.get(i) {
            Outcome::Keyboard(r) | Outcome::Gamepad(r) | Outcome::Consumer(r) => {
                // A full ring must not drop a keyboard, gamepad or consumer
                // report: flush to make room, then queue.
                if !out.push(r) {
                    if !out.flush(t).await {
                        return Step::Return(Flow::Continue);
                    }
                    out.push(r);
                }
            }
            Outcome::Sync => {
                out.drain_frame(state);
                if out.len > 0 {
                    out.armed = true;
                }
                // A frame boundary is necessary for a flush in every mode;
                // whether it is sufficient depends on the batching policy (§7.2c).
                let due = match out.interval {
                    // Timed: send only once the interval has elapsed.
                    Some(iv) => out.last_flush.elapsed() >= iv,
                    // Untimed (`none` and `adaptive`) flush every frame. Under
                    // `adaptive` the coalescing comes for free: while the previous
                    // send had the loop suspended, arriving events piled up in the
                    // channel and merged into the ring on the way here.
                    None => true,
                };
                if (due || out.is_full()) && !out.flush(t).await {
                    return Step::Return(Flow::Continue);
                }
            }
            Outcome::DropSession => {
                out.clear();
                state.clear_mouse();
                t.release_all(state).await;
                state.clear_consumer();
                t.drop_session().await;
                return Step::Return(Flow::Continue);
            }
            Outcome::Exit => {
                out.clear();
                state.clear_mouse();
                t.release_all(state).await;
                state.clear_consumer();
                return Step::Return(Flow::Shutdown);
            }
            Outcome::CaptureOff => {
                // Release everything host-side so nothing stays stuck while
                // forwarding is paused.
                out.clear();
                state.clear_mouse();
                t.release_all(state).await;
                state.clear_consumer();
            }
            Outcome::CaptureOn | Outcome::Nothing => {}
        }
    }
    Step::Continue
}

/// A transport chosen at startup. Dispatches [`Transport`] to the concrete
/// classic or LE implementation so the main loop can stay monomorphic.
pub enum AnyTransport {
    Classic(Classic),
    Le(Le),
}

impl Transport for AnyTransport {
    async fn wait_connected(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Accept {
        match self {
            Self::Classic(t) => t.wait_connected(rx, state, ctx, signals).await,
            Self::Le(t) => t.wait_connected(rx, state, ctx, signals).await,
        }
    }

    async fn run_session(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Flow {
        match self {
            Self::Classic(t) => t.run_session(rx, state, ctx, signals).await,
            Self::Le(t) => t.run_session(rx, state, ctx, signals).await,
        }
    }

    async fn send_report(&self, report: &[u8]) -> bool {
        match self {
            Self::Classic(t) => t.send_report(report).await,
            Self::Le(t) => t.send_report(report).await,
        }
    }

    async fn on_connected(&self, state: &InputState) {
        match self {
            Self::Classic(t) => t.on_connected(state).await,
            Self::Le(t) => t.on_connected(state).await,
        }
    }

    async fn drop_session(&self) {
        match self {
            Self::Classic(t) => t.drop_session().await,
            Self::Le(t) => t.drop_session().await,
        }
    }

    fn flush_interval(&self) -> Duration {
        match self {
            Self::Classic(t) => t.flush_interval(),
            Self::Le(t) => t.flush_interval(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AxisBits;

    fn outbox(capacity: usize) -> Outbox {
        Outbox::new(
            capacity,
            Batch::Auto,
            Duration::from_millis(8),
            Overflow::Burst,
        )
    }

    fn mouse(buttons: u8, x: i8, y: i8) -> Report {
        Report::new(&[0xA1, 0x01, buttons, x as u8, y as u8, 0])
    }

    fn queued(out: &Outbox) -> Vec<Vec<u8>> {
        (0..out.len)
            .map(|i| out.q[(out.head + i) % out.q.len()].as_slice().to_vec())
            .collect()
    }

    /// Consecutive motion with the same buttons collapses into one entry — this
    /// is what bounds the lag on a slow link.
    #[test]
    fn consecutive_motion_merges() {
        let mut out = outbox(4);
        for _ in 0..10 {
            assert!(out.push(mouse(0, 3, -2)));
        }
        assert_eq!(out.len, 1);
        assert_eq!(queued(&out), [vec![0xA1, 0x01, 0, 30, -20i8 as u8, 0]]);
    }

    /// A keyboard report between two mouse reports blocks the merge, so nothing
    /// can overtake it.
    #[test]
    fn keyboard_report_blocks_the_merge() {
        let mut out = outbox(8);
        out.push(mouse(0, 5, 0));
        out.push(Report::new(&[0xA1, 0x02, 0, 4, 0, 0, 0, 0, 0, 0, 0]));
        out.push(mouse(0, 7, 0));
        assert_eq!(out.len, 3);
        assert_eq!(queued(&out)[0][3], 5);
        assert_eq!(queued(&out)[2][3], 7);
    }

    /// A button change must not be absorbed into, or absorb, motion carrying a
    /// different button state.
    #[test]
    fn button_change_blocks_the_merge() {
        let mut out = outbox(8);
        out.push(mouse(0, 5, 0));
        out.push(mouse(1, 0, 0));
        out.push(mouse(1, 4, 0));
        assert_eq!(out.len, 2);
        let q = queued(&out);
        assert_eq!(q[0], [0xA1, 0x01, 0, 5, 0, 0]);
        assert_eq!(q[1], [0xA1, 0x01, 1, 4, 0, 0]);
    }

    /// A merge that would saturate the axis is refused and queued separately,
    /// so no motion is silently lost.
    #[test]
    fn saturating_merge_is_refused() {
        let mut out = outbox(4);
        out.push(mouse(0, 100, 0));
        out.push(mouse(0, 100, 0));
        assert_eq!(out.len, 2);
        assert_eq!(
            queued(&out),
            [
                vec![0xA1, 0x01, 0, 100, 0, 0],
                vec![0xA1, 0x01, 0, 100, 0, 0]
            ]
        );
    }

    /// 16-bit reports merge on their own wider range.
    #[test]
    fn sixteen_bit_reports_merge() {
        let mut out = outbox(4);
        let wide = |x: i16| {
            let [lo, hi] = x.to_le_bytes();
            Report::new(&[0xA1, 0x01, 0, lo, hi, 0, 0, 0])
        };
        out.push(wide(300));
        out.push(wide(300));
        assert_eq!(out.len, 1);
        assert_eq!(queued(&out)[0][3..5], 600i16.to_le_bytes());
    }

    /// An unmergeable report finds the ring full and is rejected, which is the
    /// signal for the caller to flush first.
    #[test]
    fn full_ring_rejects_unmergeable() {
        let mut out = outbox(2);
        assert!(out.push(mouse(0, 1, 0)));
        assert!(out.push(mouse(1, 1, 0)));
        assert!(!out.push(mouse(2, 1, 0)));
    }

    /// `Overflow` decides how far past one report's range a frame is drained.
    #[test]
    fn overflow_policies() {
        let modes = [
            (Overflow::Burst, 3, 0),
            (Overflow::Carry, 1, 173),
            (Overflow::Clamp, 1, 0),
        ];
        for (mode, want_reports, want_left) in modes {
            let mut st = crate::report::InputState::default().with_pointer(AxisBits::Eight, mode);
            let hk = crate::config::Hotkeys::default();
            crate::report::translate(
                &hk,
                &mut st,
                crate::report::RawEvent {
                    type_: crate::report::EV_REL,
                    code: crate::keymap::REL_X,
                    value: 300,
                    gamepad: None,
                },
                &mut Outcomes::default(),
            );
            let mut out = Outbox::new(8, Batch::None, Duration::from_millis(8), mode);
            out.drain_frame(&mut st);
            assert_eq!(out.len, want_reports, "{mode:?}");
            // Whatever is left is what the next frame would carry.
            let mut left = 0;
            while let Some(r) = st.take_mouse_frame() {
                left += i32::from(r.as_slice()[3] as i8);
            }
            assert_eq!(left, want_left, "{mode:?}");
        }
    }

    /// No deadline is armed until a frame boundary queues something, so an idle
    /// session runs no timer.
    #[test]
    fn deadline_only_when_armed() {
        let mut out = outbox(4);
        assert!(out.deadline().is_none());
        out.push(mouse(0, 1, 0));
        assert!(out.deadline().is_none());
        out.armed = true;
        assert!(out.deadline().is_some());
        // The untimed modes never arm a timer.
        let mut none = Outbox::new(4, Batch::None, Duration::from_millis(8), Overflow::Burst);
        none.armed = true;
        assert!(none.deadline().is_none());
    }
}
