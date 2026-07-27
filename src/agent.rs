//! The shared BlueZ pairing agent, registered as the default agent for whichever
//! transport is active. HID bonding needs an agent to answer pairing requests
//! (BLE HOGP requires an encrypted, bonded link; on Classic an incoming pair
//! stalls with no agent registered). Behaviour follows `[connection] pairing`:
//! `Accept` bonds silently ("Just Works"), `Prompt` asks on the TTY, and
//! `PromptIfPossible` picks between the two by whether stdin is a TTY. See
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

/// What the agent actually does once the TTY question is settled, i.e. the
/// result of [`resolve_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Accept silently ("Just Works").
    Accept,
    /// Ask on the TTY before bonding.
    Prompt,
}

/// Build the agent for the resolved pairing behaviour. `coord` lets the prompt
/// borrow the terminal from a running interactive menu before reading on the
/// TTY (design/CONNECTION.md §5/§6).
pub fn agent(mode: Resolved, coord: TermCoord) -> Agent {
    match mode {
        Resolved::Accept => auto_accept_agent(),
        Resolved::Prompt => confirm_agent(coord),
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

/// Resolve the configured pairing mode against whether stdin is a TTY.
/// `Prompt` without a TTY has no way to ask, so it is an error (reported at
/// startup) rather than a silent downgrade to accepting everything.
pub fn resolve_mode(configured: PairingMode, interactive: bool) -> Result<Resolved, String> {
    match configured {
        PairingMode::Accept => Ok(Resolved::Accept),
        PairingMode::PromptIfPossible if interactive => Ok(Resolved::Prompt),
        PairingMode::PromptIfPossible => Ok(Resolved::Accept),
        PairingMode::Prompt if interactive => Ok(Resolved::Prompt),
        PairingMode::Prompt => Err(
            "[connection] pairing = \"prompt\" needs a terminal to prompt on, but stdin is not a \
             TTY; use \"prompt_if_possible\" or \"accept\""
                .to_string(),
        ),
    }
}

/// Parse a config reconnect address string into an [`Address`]. The string was
/// already validated at config-parse time, so this only fails on a truly
/// malformed value.
pub fn parse_address(s: &str) -> Option<Address> {
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_mode_needs_a_tty_only_for_prompt() {
        for interactive in [true, false] {
            assert_eq!(
                resolve_mode(PairingMode::Accept, interactive),
                Ok(Resolved::Accept)
            );
        }
        assert_eq!(
            resolve_mode(PairingMode::PromptIfPossible, true),
            Ok(Resolved::Prompt)
        );
        assert_eq!(
            resolve_mode(PairingMode::PromptIfPossible, false),
            Ok(Resolved::Accept)
        );
        assert_eq!(
            resolve_mode(PairingMode::Prompt, true),
            Ok(Resolved::Prompt)
        );
        assert!(resolve_mode(PairingMode::Prompt, false).is_err());
    }
}
