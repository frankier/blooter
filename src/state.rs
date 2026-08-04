//! Persistent record of which HID report descriptor each host was last bonded
//! under, so a descriptor change can be spotted and fixed (design/CONNECTION.md §7).
//!
//! Hosts cache blooter's SDP record — including the HID report descriptor — for
//! the lifetime of the bond. Changing the descriptor (adding or removing gamepad
//! slots) therefore has no effect on an already-bonded host: it keeps using the
//! copy it cached when it first paired. Recording the descriptor fingerprint in
//! force at each host's last session lets blooter mark such hosts stale and
//! offer the "fix connection" action rather than silently misbehaving.
//!
//! The transport each bond was made over is recorded here too, because
//! bluetoothd does not expose it: a bond is a link key on Classic and an LTK on
//! BLE, neither carries over to the other, and a host bonded under the wrong one
//! can never connect (design/CONNECTION.md §8.1). Knowing which is what lets
//! that be said at startup rather than waited out.
//!
//! The file is one `<address> <fingerprint> [protocol]` line per host;
//! unparsable lines are skipped, a line without the third field reads as "the
//! transport is not known" (a file written by an older blooter), and every
//! failure is non-fatal (the feature degrades to "no stale markers", never to a
//! startup error).

use std::collections::HashMap;
use std::path::PathBuf;

use bluer::Address;
use log::debug;

use crate::config::Protocol;

/// What blooter remembers about one host between runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    /// The descriptor fingerprint in force at that host's last session.
    pub fingerprint: u32,
    /// The transport its bond was made over; `None` for a record written before
    /// this was tracked, which is never reported as a mismatch.
    pub protocol: Option<Protocol>,
}

/// What blooter knows about the hosts it has connected to, keyed by address.
/// Loaded at startup and rewritten whenever an entry changes.
#[derive(Default)]
pub struct Hosts {
    path: Option<PathBuf>,
    map: HashMap<Address, Record>,
}

/// `$XDG_STATE_HOME/blooter/hosts`, else `$HOME/.local/state/blooter/hosts`,
/// else `/var/lib/blooter/hosts` (the usual case under `sudo`/systemd).
fn default_path() -> Option<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if !x.is_empty() => Some(PathBuf::from(x).join("blooter/hosts")),
        _ => match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => {
                Some(PathBuf::from(home).join(".local/state/blooter/hosts"))
            }
            _ => Some(PathBuf::from("/var/lib/blooter/hosts")),
        },
    }
}

impl Hosts {
    /// Load the recorded fingerprints; an unreadable or absent file yields an
    /// empty set (every host simply reads as "unknown", never as stale).
    pub fn load() -> Self {
        let path = default_path();
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse(&text))
            .unwrap_or_default();
        Hosts { path, map }
    }

    /// Record (and persist) the fingerprint and transport `addr` is now bonded
    /// under.
    pub fn set(&mut self, addr: Address, fingerprint: u32, protocol: Protocol) {
        let record = Record {
            fingerprint,
            protocol: Some(protocol),
        };
        if self.map.insert(addr, record) != Some(record) {
            self.save();
        }
    }

    /// Forget `addr` — used when its bond is dropped, so a later re-pair starts
    /// from "unknown" rather than a fingerprint that no longer applies.
    pub fn forget(&mut self, addr: Address) {
        if self.map.remove(&addr).is_some() {
            self.save();
        }
    }

    /// The addresses whose recorded fingerprint differs from `current` — hosts
    /// holding a cached copy of an older descriptor. Hosts with no record are
    /// not included: unknown is not the same as stale.
    pub fn stale(&self, current: u32) -> Vec<Address> {
        let mut stale: Vec<Address> = self
            .map
            .iter()
            .filter(|(_, r)| r.fingerprint != current)
            .map(|(addr, _)| *addr)
            .collect();
        stale.sort_by_key(|a| a.0); // stable order for logs and menu markers
        stale
    }

    /// Every record, in the same stable address order `stale` and `addresses`
    /// give. The startup audit walks these against bluetoothd's bonds
    /// (design/CONNECTION.md §8.2).
    pub fn records(&self) -> Vec<(Address, Record)> {
        let mut records: Vec<(Address, Record)> = self.map.iter().map(|(a, r)| (*a, *r)).collect();
        records.sort_by_key(|(a, _)| a.0);
        records
    }

    /// Every host with a record, whatever its fingerprint. The BLE menu unions
    /// this with bluetoothd's bonded devices, so a host whose device object
    /// bluetoothd has dropped still has a row to act on (design/CONNECTION.md §6).
    pub fn addresses(&self) -> Vec<Address> {
        let mut addrs: Vec<Address> = self.map.keys().copied().collect();
        addrs.sort_by_key(|a| a.0); // stable order, as `stale` gives
        addrs
    }

    /// Rewrite the file. Failures are logged at debug and otherwise ignored —
    /// losing this state costs a stale marker, not correctness.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut text = String::new();
        for (addr, r) in &self.map {
            text.push_str(&format!("{addr} {:08x}", r.fingerprint));
            if let Some(p) = r.protocol {
                text.push_str(&format!(" {}", protocol_name(p)));
            }
            text.push('\n');
        }
        let res = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, text));
        if let Err(e) = res {
            debug!("cannot write {}: {e}", path.display());
        }
    }
}

/// The name a transport is written under, and read back by [`parse`].
pub fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Classic => "classic",
        Protocol::Ble => "ble",
    }
}

/// Parse `<address> <hex fingerprint> [protocol]` lines, skipping anything
/// malformed. A missing or unrecognised protocol field reads as "not known",
/// which is how a file written by an older blooter parses.
fn parse(text: &str) -> HashMap<Address, Record> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let addr = fields.next()?.parse().ok()?;
            let fingerprint = u32::from_str_radix(fields.next()?, 16).ok()?;
            let protocol = match fields.next() {
                Some("classic") => Some(Protocol::Classic),
                Some("ble") => Some(Protocol::Ble),
                _ => None,
            };
            Some((
                addr,
                Record {
                    fingerprint,
                    protocol,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::new([0, 0, 0, 0, 0, n])
    }

    fn record(fingerprint: u32) -> Record {
        Record {
            fingerprint,
            protocol: None,
        }
    }

    #[test]
    fn parses_and_skips_junk() {
        let map = parse(
            "00:00:00:00:00:01 deadbeef classic\nnonsense\n\n\
             00:00:00:00:00:02 0000000f ble\nzz ff\n",
        );
        assert_eq!(map.len(), 2);
        assert_eq!(map[&addr(1)].fingerprint, 0xdead_beef);
        assert_eq!(map[&addr(1)].protocol, Some(Protocol::Classic));
        assert_eq!(map[&addr(2)].fingerprint, 0x0f);
        assert_eq!(map[&addr(2)].protocol, Some(Protocol::Ble));
    }

    #[test]
    fn a_line_without_a_transport_is_unknown_not_a_mismatch() {
        // What a file written by an older blooter looks like: the fingerprint
        // still applies, the transport is simply not known, and "unknown" must
        // never be reported as bonded over the wrong one.
        let map = parse("00:00:00:00:00:01 deadbeef\n00:00:00:00:00:02 0f nonsense\n");
        assert_eq!(map[&addr(1)], record(0xdead_beef));
        assert_eq!(map[&addr(2)], record(0x0f));
    }

    #[test]
    fn stale_lists_only_mismatches() {
        let mut hosts = Hosts::default();
        hosts.map.insert(addr(1), record(1));
        hosts.map.insert(addr(2), record(2));
        let stale = hosts.stale(2);
        assert_eq!(stale, vec![addr(1)]);
        // A host with no record is unknown, not stale.
        assert!(!hosts.map.contains_key(&addr(3)));
        assert!(!hosts.stale(2).contains(&addr(3)));
    }

    #[test]
    fn set_records_the_transport() {
        let mut hosts = Hosts::default();
        hosts.set(addr(1), 7, Protocol::Ble);
        assert_eq!(
            hosts.records(),
            vec![(
                addr(1),
                Record {
                    fingerprint: 7,
                    protocol: Some(Protocol::Ble),
                }
            )]
        );
    }

    #[test]
    fn forget_drops_the_record() {
        let mut hosts = Hosts::default();
        hosts.map.insert(addr(1), record(7));
        hosts.forget(addr(1));
        assert!(!hosts.map.contains_key(&addr(1)));
    }
}
