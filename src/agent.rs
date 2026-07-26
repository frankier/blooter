//! The shared BlueZ pairing agent, registered as the default agent for whichever
//! transport is active. HID bonding needs an agent to answer pairing requests
//! (BLE HOGP requires an encrypted, bonded link; on Classic an incoming pair
//! stalls with no agent registered). Behaviour follows `[connection] pairing`:
//! `Auto` accepts silently ("Just Works"), `Confirm` prompts on the TTY. See
//! design/CONNECTION.md §5.

use std::io::{self, Write};
use std::pin::Pin;

use bluer::Address;
use bluer::agent::{Agent, ReqError, ReqResult};
use futures::FutureExt;
use log::info;

use crate::config::PairingMode;
use crate::menu::TermCoord;

type ReqFuture = Pin<Box<dyn std::future::Future<Output = ReqResult<()>> + Send>>;

/// Build the agent for the given pairing mode. `coord` lets the confirm-mode
/// prompt borrow the terminal from a running interactive menu before reading on
/// the TTY (design/CONNECTION.md §5/§6).
pub fn agent(mode: PairingMode, coord: TermCoord) -> Agent {
    match mode {
        PairingMode::Auto => auto_accept_agent(),
        PairingMode::Confirm => confirm_agent(coord),
    }
}

/// A pairing agent that accepts "Just Works" bonding without interaction. The
/// callbacks it provides (no pin/passkey entry) keep the negotiated capability
/// at NoInputNoOutput, forcing Just Works. Bonds persist in BlueZ.
fn auto_accept_agent() -> Agent {
    fn ok() -> ReqFuture {
        async move { Ok(()) }.boxed()
    }
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(|_| ok())),
        request_authorization: Some(Box::new(|_| ok())),
        authorize_service: Some(Box::new(|_| ok())),
        ..Default::default()
    }
}

/// A pairing agent that asks the user on the TTY before bonding. Declining
/// rejects the request. Service authorization is accepted (the host has already
/// been confirmed as a pairing peer).
fn confirm_agent(coord: TermCoord) -> Agent {
    let confirm_coord = coord.clone();
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let msg = format!(
                "Confirm pairing with {} (passkey {:06})",
                req.device, req.passkey
            );
            prompt(confirm_coord.clone(), msg)
        })),
        request_authorization: Some(Box::new(move |req| {
            prompt(coord.clone(), format!("Allow pairing with {}", req.device))
        })),
        authorize_service: Some(Box::new(|_| async move { Ok(()) }.boxed())),
        ..Default::default()
    }
}

/// Ask `<msg> [Y/n]?` on the TTY; empty/`y` accepts, anything else rejects.
/// Reads on a blocking thread so the async runtime is not stalled. Borrows the
/// terminal from any running menu first (so its `EventStream` does not swallow
/// the reply) and prints a leading newline to break away from the menu's last
/// line (design/CONNECTION.md §5/§6).
fn prompt(coord: TermCoord, msg: String) -> ReqFuture {
    async move {
        // Suspend the menu (if any) for the duration of the read; `_borrow`
        // resumes it on drop at the end of this future.
        let _borrow = coord.borrow().await;
        let answer = tokio::task::spawn_blocking(move || {
            print!("\n{msg} [Y/n]? ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            io::stdin().read_line(&mut line).map(|_| line)
        })
        .await;
        match answer {
            Ok(Ok(line)) if matches!(line.trim(), "" | "y" | "Y" | "yes") => {
                info!("pairing confirmed by user");
                Ok(())
            }
            _ => {
                info!("pairing declined by user");
                Err(ReqError::Rejected)
            }
        }
    }
    .boxed()
}

/// Resolve the effective pairing mode: the explicit config value, else inferred
/// from whether stdin is a TTY (`Confirm` interactively, `Auto` otherwise).
pub fn resolve_mode(configured: Option<PairingMode>, interactive: bool) -> PairingMode {
    configured.unwrap_or(if interactive {
        PairingMode::Confirm
    } else {
        PairingMode::Auto
    })
}

/// Parse a config reconnect address string into an [`Address`]. The string was
/// already validated at config-parse time, so this only fails on a truly
/// malformed value.
pub fn parse_address(s: &str) -> Option<Address> {
    s.parse().ok()
}
