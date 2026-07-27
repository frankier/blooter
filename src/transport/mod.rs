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

use crate::report::{InputState, Outcome, RawEvent};
use crate::{Ctx, Signals};
use std::time::Duration;
use tokio::sync::mpsc;

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

    /// Release all inputs host-side: an all-keys-up keyboard report plus a
    /// neutral report for every advertised gamepad, so nothing stays latched
    /// when a session is dropped, blooter exits, or capture is paused.
    async fn release_all(&self, state: &InputState) {
        self.send_report(&InputState::keys_up_report()).await;
        for r in state.gamepad_neutral_reports() {
            self.send_report(&r).await;
        }
    }
}

/// Translate one event and act on the result, shared by every transport's
/// session loop. Sends report outcomes via [`Transport::send_report`] and maps
/// hotkey outcomes (drop/exit/capture) onto the session control flow.
pub async fn dispatch<T: Transport>(
    t: &T,
    ctx: &Ctx<'_>,
    state: &mut InputState,
    ev: RawEvent,
) -> Step {
    match ctx.translate(state, ev) {
        Outcome::Mouse(r) => {
            if !t.send_report(&r).await {
                return Step::Return(Flow::Continue);
            }
        }
        Outcome::Keyboard(r) => {
            if !t.send_report(&r).await {
                return Step::Return(Flow::Continue);
            }
        }
        Outcome::Gamepad(r) => {
            if !t.send_report(&r).await {
                return Step::Return(Flow::Continue);
            }
        }
        Outcome::DropSession => {
            t.release_all(state).await;
            return Step::Return(Flow::Continue);
        }
        Outcome::Exit => {
            t.release_all(state).await;
            return Step::Return(Flow::Shutdown);
        }
        Outcome::CaptureOff => {
            // Release everything host-side so nothing stays stuck while
            // forwarding is paused.
            t.release_all(state).await;
        }
        Outcome::CaptureOn | Outcome::Nothing => {}
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
}
