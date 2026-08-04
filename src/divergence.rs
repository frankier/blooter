//! Noticing that the two halves of a bond no longer agree, and saying so
//! (design/CONNECTION.md §8).
//!
//! Bonding state lives on two machines and in two daemons; blooter owns one
//! half. Every failure in §8.1 leaves that half looking perfectly healthy, so
//! the fault is only ever visible in the *disagreement* — and blooter is the
//! only process positioned to see it. The principle §8 draws from that is the
//! whole of this module: **a setup that cannot work should never present as one
//! that is merely waiting.**
//!
//! Two entry points, because the facts arrive at two different times:
//!
//! - [`audit`] runs at startup, before a host that can never connect is waited
//!   for. Everything it checks is local: what [`crate::state::Hosts`] recorded
//!   about each bond against what bluetoothd still holds and what
//!   `[connection] protocol` now says.
//! - [`watch`] runs for the life of the process, because a bond store can be
//!   damaged while blooter is running — and on BLE the damage arrives as
//!   nothing at all (see [`ADVISE_AFTER`]).
//!
//! Every message names the machine, says which side holds the stale half, and
//! gives the exact remedy (§8.2 point 2). Nothing here ever repairs anything:
//! detection is free and always on, repair is explicit and behind a keystroke
//! (§8.2 point 4).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bluer::{Adapter, AdapterEvent, Address, Device, DeviceEvent, DeviceProperty};
use futures::StreamExt;
use log::{debug, warn};
use tokio::sync::mpsc;

use crate::config::Protocol;
use crate::state::{Hosts, protocol_name};

/// How long blooter waits, holding bonds with nothing connected, before saying
/// what "advertising" may actually mean.
///
/// On BLE the damage of D1 — the host deleting blooter from its Bluetooth
/// settings — reaches this machine as *no signal whatsoever*: SMP has no
/// "I have forgotten you" message, and a central that no longer knows blooter
/// never dials it again. Silence is therefore the only symptom there is, and
/// §8's principle says silence must not be reported as "advertising as blooter"
/// indefinitely. The message is phrased as the conditional it is.
const ADVISE_AFTER: Duration = Duration::from_secs(15);

/// A link that comes up and drops again inside this is not a session; it is an
/// encrypted connection that could not be established. That is exactly what a
/// bonded host holding no key looks like from here, and unlike the silence
/// above it is a real signal.
const TOO_SHORT: Duration = Duration::from_secs(3);

/// How often the idle advisory is re-evaluated. Off every hot path — this task
/// only ever runs while no input is being forwarded.
const TICK: Duration = Duration::from_secs(5);

/// The remedy blooter cannot perform itself, spelled out (§8.2 point 2: "a
/// repair the user cannot perform from blooter's side must say so plainly").
/// One line, because it has to be readable next to the address it follows.
const HOST_SIDE_REMEDY: &str =
    "Remove blooter from that host's Bluetooth settings, then pair again from there.";

/// The menu key that drops blooter's half of a bond on this transport. Classic
/// has no `[u]`: there, dropping our half is what `[f]` does on the way to
/// unplugging the host (§6, §7.2a).
fn drop_key(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Ble => "[u]",
        Protocol::Classic => "[f]",
    }
}

/// Whether bluetoothd still holds a bond for `addr`.
async fn is_bonded(adapter: &Adapter, addr: Address) -> bool {
    match adapter.device(addr) {
        Ok(device) => device.is_paired().await.unwrap_or(false),
        Err(_) => false,
    }
}

/// The addresses bluetoothd still holds a bond for.
async fn bonded(adapter: &Adapter) -> HashSet<Address> {
    let mut bonded = HashSet::new();
    for addr in adapter.device_addresses().await.unwrap_or_default() {
        if let Ok(device) = adapter.device(addr)
            && device.is_paired().await.unwrap_or(false)
        {
            bonded.insert(addr);
        }
    }
    bonded
}

/// The §8.2.1 startup check: every bonded host's transport matches the
/// configured protocol, and every host blooter remembers is still bonded.
///
/// Both are entirely local — blooter knows the configured protocol, and it
/// recorded the transport of each bond as it was made ([`crate::state`]) — so
/// neither needs a host to show up, which is the point: the user is told before
/// waiting rather than after a failure that names nothing.
pub async fn audit(adapter: Option<&Adapter>, protocol: Protocol, hosts: &Mutex<Hosts>) {
    let Some(adapter) = adapter else { return };
    let records = hosts.lock().unwrap().records();
    if records.is_empty() {
        return; // nothing remembered, so nothing can disagree
    }
    let bonded = bonded(adapter).await;

    let mut lost = Vec::new();
    for (addr, record) in records {
        match record.protocol {
            // A bond is a link key on Classic and an LTK on BLE; neither is any
            // use to the other bearer, so this host will dial a transport
            // blooter is no longer on and fail there (§8.1).
            Some(was) if was != protocol => warn!(
                "{addr}: that bond was made over {}, but [connection] protocol is now {}. A bond \
                 does not carry over between transports, so this host cannot connect. \
                 {HOST_SIDE_REMEDY}",
                transport_label(was),
                protocol_name(protocol),
            ),
            _ if !bonded.contains(&addr) => lost.push(addr),
            _ => {}
        }
    }

    if !lost.is_empty() {
        let list: Vec<String> = lost.iter().map(Address::to_string).collect();
        let prefix = if bonded.is_empty() {
            "no bonded hosts: "
        } else {
            ""
        };
        warn!(
            "{prefix}blooter's half of the bond with {} is gone, but that host probably still \
             holds its half, and a host that does cannot reconnect. {HOST_SIDE_REMEDY}",
            list.join(", ")
        );
    }
}

/// The transport under the name a user would recognise from a Bluetooth panel,
/// rather than the config token [`protocol_name`] gives.
fn transport_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Classic => "Classic (BR/EDR)",
        Protocol::Ble => "BLE (HID-over-GATT)",
    }
}

/// What one device's watcher reports.
enum Change {
    /// `Device.Connected` changed.
    Connected(Address, bool),
    /// The bond is gone: `Paired` went false, or the device object was removed.
    Unbonded(Address),
}

/// Watch bluetoothd for the divergences that appear *while* blooter runs, for
/// the life of the process (design/CONNECTION.md §8).
///
/// Three things are reported, and none of them is repaired:
///
/// 1. **Our half disappears** (D2/D6 out of band). A bond blooter recorded that
///    bluetoothd no longer holds means the host is still carrying a key for a
///    peer that has forgotten it. Removals blooter performed itself are silent,
///    because `[u]`/`[f]` drop the state record *before* the bond and the menu
///    has already said its piece.
/// 2. **A link that will not stay up** (D1 on BLE, when the host does try).
///    A bonded host whose connection dies inside [`TOO_SHORT`] failed at
///    encryption; it no longer has the key blooter still holds.
/// 3. **Nothing at all** — see [`ADVISE_AFTER`].
pub async fn watch(adapter: Adapter, hosts: Arc<Mutex<Hosts>>, protocol: Protocol) {
    let Ok(mut events) = adapter.events().await else {
        debug!("cannot watch for bond divergence; startup checks only");
        return;
    };

    // One task per device streams its properties here, so this loop holds the
    // whole picture — the same shape `Le::watch_links` uses for the link edge.
    let (tx, mut rx) = mpsc::channel::<Change>(16);
    for addr in adapter.device_addresses().await.unwrap_or_default() {
        if let Ok(device) = adapter.device(addr) {
            tokio::spawn(watch_device(device, tx.clone()));
        }
    }

    let mut up: HashMap<Address, Instant> = HashMap::new();
    // Reported once per run per host: a device that flaps must not become a
    // wall of identical warnings.
    let mut said: HashSet<Address> = HashSet::new();
    // When the last link went away (or startup, for a blooter that never sees
    // one), and whether the idle advisory has already been given for it.
    let mut idle_since = Instant::now();
    let mut advised = false;
    let mut tick = tokio::time::interval(TICK);

    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(AdapterEvent::DeviceAdded(addr)) => {
                    if let Ok(device) = adapter.device(addr) {
                        tokio::spawn(watch_device(device, tx.clone()));
                    }
                }
                // Removal ends the device's own stream, which reports it.
                Some(_) => {}
                None => break, // adapter gone
            },
            Some(change) = rx.recv() => match change {
                Change::Connected(addr, true) => {
                    up.insert(addr, Instant::now());
                    // A host that connects is a host that still knows blooter.
                    said.remove(&addr);
                    advised = false;
                }
                Change::Connected(addr, false) => {
                    let held = up.remove(&addr).map(|t| t.elapsed());
                    if up.is_empty() {
                        idle_since = Instant::now();
                    }
                    if held.is_some_and(|h| h < TOO_SHORT)
                        && !said.contains(&addr)
                        && is_bonded(&adapter, addr).await
                    {
                        said.insert(addr);
                        warn!(
                            "{addr} connected and the link died immediately: it no longer has \
                             the key blooter still holds for it, so it will not connect. Press \
                             {} here to drop blooter's half. {HOST_SIDE_REMEDY}",
                            drop_key(protocol),
                        );
                    }
                }
                Change::Unbonded(addr) => {
                    // Recorded but no longer bonded: our half went out of band.
                    // A removal blooter did itself has already forgotten the
                    // record, so this stays quiet for `[u]` and `[f]`.
                    let recorded = hosts.lock().unwrap().addresses().contains(&addr);
                    if recorded && said.insert(addr) {
                        warn!(
                            "{addr}: blooter's half of the bond is gone, but that host probably \
                             still holds its half, and a host that does cannot reconnect. \
                             {HOST_SIDE_REMEDY}"
                        );
                    }
                }
            },
            _ = tick.tick() => {
                if advised || !up.is_empty() || idle_since.elapsed() < ADVISE_AFTER {
                    continue;
                }
                advised = true;
                for addr in bonded(&adapter).await {
                    warn!(
                        "{addr} is bonded here but is not connecting. If blooter was removed \
                         from that machine's Bluetooth settings it no longer knows blooter and \
                         will not connect: press {} here to drop blooter's half, then pair \
                         again from that host.",
                        drop_key(protocol),
                    );
                }
            }
        }
    }
}

/// Report one device's bond and link changes to [`watch`] until it is removed,
/// which counts as both a disconnection and the loss of the bond.
///
/// The stream is opened *before* the initial state is read, so a change racing
/// startup is seen as an event rather than lost between the two — the same
/// ordering `le::watch_device` depends on.
async fn watch_device(device: Device, tx: mpsc::Sender<Change>) {
    let addr = device.address();
    let Ok(mut events) = device.events().await else {
        return;
    };
    if device.is_connected().await.unwrap_or(false)
        && tx.send(Change::Connected(addr, true)).await.is_err()
    {
        return;
    }
    while let Some(event) = events.next().await {
        let change = match event {
            DeviceEvent::PropertyChanged(DeviceProperty::Connected(c)) => {
                Change::Connected(addr, c)
            }
            DeviceEvent::PropertyChanged(DeviceProperty::Paired(false)) => Change::Unbonded(addr),
            _ => continue,
        };
        if tx.send(change).await.is_err() {
            return;
        }
    }
    // The object is gone, which is what `bluetoothctl remove` leaves behind.
    let _ = tx.send(Change::Connected(addr, false)).await;
    let _ = tx.send(Change::Unbonded(addr)).await;
}
