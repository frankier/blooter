//! The shared BlueZ pairing agent, registered as the default agent for whichever
//! transport is active. HID bonding needs an agent to answer pairing requests
//! (BLE HOGP requires an encrypted, bonded link; on Classic an incoming pair
//! stalls with no agent registered). Behaviour follows `[connection] pairing`:
//! `Accept` bonds silently ("Just Works"), `Prompt` asks on the TTY, and
//! `PromptIfPossible` picks between the two by whether stdin is a TTY. See
//! design/CONNECTION.md §5.
//!
//! **The registered IO capability is what actually decides the pairing model**,
//! and bluer derives it from *which callbacks are set* rather than from anything
//! declared here (`bluer::agent::Agent::capability`). So the callback set of each
//! agent below is load-bearing, not a convenience — adding a handler to one of
//! them silently changes how every host pairs:
//!
//! | Agent | Callbacks | Capability | Model |
//! |---|---|---|---|
//! | [`just_works_agent`] | none | `NoInputNoOutput` | Just Works, no interaction |
//! | [`auto_accept_agent`] | confirm/authorize | `DisplayYesNo` | host may pick passkey entry |
//! | [`interactive_agent`] | all of them | `KeyboardDisplay` | any model, answered on the TTY |
//!
//! This is why `Accept` is transport-specific. BLE needs the real
//! `NoInputNoOutput` to get Just Works: with `DisplayYesNo` a host takes blooter
//! for a device that can show and type digits and picks Passkey Entry, which a
//! callback-less agent cannot answer — the host shows a PIN and blooter shows
//! nothing. Classic keeps the `DisplayYesNo` set because `AuthorizeService` is a
//! BR/EDR profile-authorization call that a bare agent would reject; LE never
//! makes it.

use std::io::{self, Write};
use std::pin::Pin;

use bluer::Address;
use bluer::agent::{Agent, ReqError, ReqResult};
use futures::FutureExt;
use log::info;

use crate::config::{PairingMode, Protocol};
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

/// Build the agent for the resolved pairing behaviour on `protocol`. `coord`
/// lets a prompt borrow the terminal from a running interactive menu before
/// reading on the TTY (design/CONNECTION.md §5/§6).
pub fn agent(mode: Resolved, protocol: Protocol, coord: TermCoord) -> Agent {
    match (mode, protocol) {
        (Resolved::Accept, Protocol::Ble) => just_works_agent(),
        (Resolved::Accept, Protocol::Classic) => auto_accept_agent(),
        (Resolved::Prompt, _) => interactive_agent(coord),
    }
}

/// A pairing agent with no callbacks at all, which registers as
/// `NoInputNoOutput` and so pins the negotiated model to **Just Works**: BlueZ
/// bonds without ever calling back. This is what makes BLE pairing work with
/// nothing but blooter running — no tray, no prompt, nothing to answer.
///
/// It must stay callback-free. Any handler added here changes the advertised
/// capability and lets the host choose a model that needs answering.
fn just_works_agent() -> Agent {
    Agent {
        request_default: true,
        ..Default::default()
    }
}

/// A pairing agent that accepts bonding without interaction, and additionally
/// authorizes incoming service connections — which Classic needs, since
/// bluetoothd asks before letting an untrusted device reach the HID PSMs.
///
/// Providing those callbacks registers as `DisplayYesNo`, so a Classic host may
/// still pick a passkey model; SSP numeric comparison is confirmed silently here.
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

/// A pairing agent that answers every association model on the TTY, registering
/// as `KeyboardDisplay`. Covering all four callbacks is the point: it is what
/// lets blooter itself display a passkey the host asks it to show, and type one
/// the host displays, so pairing never depends on a desktop Bluetooth agent
/// being around. Declining rejects the request.
fn interactive_agent(coord: TermCoord) -> Agent {
    let (confirm, authorize) = (coord.clone(), coord.clone());
    let (show_passkey, show_pin) = (coord.clone(), coord.clone());
    let ask_passkey = coord.clone();
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let msg = format!(
                "Confirm pairing with {} (passkey {:06})",
                req.device, req.passkey
            );
            confirm_prompt(confirm.clone(), msg)
        })),
        request_authorization: Some(Box::new(move |req| {
            confirm_prompt(
                authorize.clone(),
                format!("Allow pairing with {}", req.device),
            )
        })),
        // The host displays, we type. Without these two the host shows a PIN
        // and blooter has no way to answer it.
        request_passkey: Some(Box::new(move |req| {
            let coord = ask_passkey.clone();
            async move {
                let msg = format!("Enter the passkey shown on {}", req.device);
                match ask(coord, msg).await.as_deref().map(str::parse::<u32>) {
                    Some(Ok(p)) if p <= 999_999 => {
                        info!("passkey entered by user");
                        Ok(p)
                    }
                    _ => {
                        info!("no usable passkey entered; rejecting");
                        Err(ReqError::Rejected)
                    }
                }
            }
            .boxed()
        })),
        request_pin_code: Some(Box::new(move |req| {
            let coord = coord.clone();
            async move {
                let msg = format!("Enter the PIN shown on {}", req.device);
                match ask(coord, msg).await {
                    // BlueZ wants 1-16 characters; anything else is a rejection
                    // rather than a call it would refuse anyway.
                    Some(pin) if (1..=16).contains(&pin.len()) => Ok(pin),
                    _ => Err(ReqError::Rejected),
                }
            }
            .boxed()
        })),
        // We display, the host types. BlueZ resolves `cancel` when the value
        // should come off screen, so the borrow is held until then and the
        // digits are not overwritten by a menu repaint.
        display_passkey: Some(Box::new(move |req| {
            let coord = show_passkey.clone();
            async move {
                let msg = format!(
                    "Type this passkey on {} to pair: {:06}",
                    req.device, req.passkey
                );
                show(coord, msg, req.cancel).await;
                Ok(())
            }
            .boxed()
        })),
        display_pin_code: Some(Box::new(move |req| {
            let coord = show_pin.clone();
            async move {
                let msg = format!("Type this PIN on {} to pair: {}", req.device, req.pincode);
                show(coord, msg, req.cancel).await;
                Ok(())
            }
            .boxed()
        })),
        authorize_service: Some(Box::new(|_| async move { Ok(()) }.boxed())),
        ..Default::default()
    }
}

/// Ask `<msg> [Y/n]?` on the TTY; empty/`y` accepts, anything else rejects.
fn confirm_prompt(coord: TermCoord, msg: String) -> ReqFuture {
    async move {
        match ask(coord, format!("{msg} [Y/n]?")).await.as_deref() {
            Some("" | "y" | "Y" | "yes") => {
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

/// Print `msg` on the TTY and read one trimmed line back, or `None` if stdin
/// could not be read. Reads on a blocking thread so the async runtime is not
/// stalled. Borrows the terminal from any running menu first (so its
/// `EventStream` does not swallow the reply) and prints a leading newline to
/// break away from the menu's last line (design/CONNECTION.md §5/§6).
async fn ask(coord: TermCoord, msg: String) -> Option<String> {
    // Suspend the menu (if any) for the duration of the read; `_borrow` resumes
    // it on drop at the end of this scope.
    let _borrow = coord.borrow().await;
    tokio::task::spawn_blocking(move || {
        print!("\n{msg} ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line).ok()?;
        Some(line.trim().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Print `msg` on the TTY and leave it there until BlueZ says the value need no
/// longer be displayed. The menu stays suspended for that whole time, so it
/// cannot repaint over the digits the user is copying.
async fn show(coord: TermCoord, msg: String, cancel: tokio::sync::oneshot::Receiver<()>) {
    let _borrow = coord.borrow().await;
    println!("\n{msg}");
    let _ = io::stdout().flush();
    let _ = cancel.await;
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

    /// bluer's `Agent::capability()` is `pub(crate)`, so mirror its mapping
    /// (`bluer::agent`, the `match (keyboard, display_only, yes_no)`) here. The
    /// value it computes is what BlueZ negotiates the association model from,
    /// so it is worth pinning even at the cost of restating the rule.
    fn capability(a: &Agent) -> &'static str {
        let keyboard = a.request_passkey.is_some() || a.request_pin_code.is_some();
        let display_only = a.display_passkey.is_some() || a.display_pin_code.is_some();
        let yes_no = a.request_confirmation.is_some()
            || a.request_authorization.is_some()
            || a.authorize_service.is_some();
        match (keyboard, display_only, yes_no) {
            (true, false, false) => "KeyboardOnly",
            (false, true, false) => "DisplayOnly",
            (false, _, true) => "DisplayYesNo",
            (true, true, _) | (true, _, true) => "KeyboardDisplay",
            (false, false, false) => "NoInputNoOutput",
        }
    }

    /// The capability is a side effect of the callback set, so a stray handler
    /// would silently change how every host pairs. BLE in particular *must* be
    /// `NoInputNoOutput`: that is the whole reason it can pair non-interactively.
    #[test]
    fn each_mode_registers_the_intended_capability() {
        let coord = TermCoord::default();
        for (mode, protocol, want) in [
            (Resolved::Accept, Protocol::Ble, "NoInputNoOutput"),
            (Resolved::Accept, Protocol::Classic, "DisplayYesNo"),
            (Resolved::Prompt, Protocol::Ble, "KeyboardDisplay"),
            (Resolved::Prompt, Protocol::Classic, "KeyboardDisplay"),
        ] {
            let a = agent(mode, protocol, coord.clone());
            assert_eq!(capability(&a), want, "{mode:?} on {protocol:?}");
            assert!(a.request_default, "the agent must be the default one");
        }
    }

    /// Prompting is only useful if blooter can answer every model itself, which
    /// is the point of not depending on a desktop Bluetooth agent.
    #[test]
    fn the_interactive_agent_answers_every_model() {
        let a = interactive_agent(TermCoord::default());
        assert!(a.request_confirmation.is_some(), "numeric comparison");
        assert!(a.request_authorization.is_some(), "just works confirmation");
        assert!(a.request_passkey.is_some(), "passkey entry (host displays)");
        assert!(a.display_passkey.is_some(), "passkey display (host types)");
        assert!(a.request_pin_code.is_some(), "legacy pin entry");
        assert!(a.display_pin_code.is_some(), "legacy pin display");
    }

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
