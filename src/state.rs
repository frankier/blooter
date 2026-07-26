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
//! The file is one `<address> <fingerprint>` line per host; unparsable lines are
//! skipped, and every failure is non-fatal (the feature degrades to "no stale
//! markers", never to a startup error).

use std::collections::HashMap;
use std::path::PathBuf;

use bluer::Address;
use log::debug;

/// Descriptor fingerprints of the hosts blooter has connected to, keyed by
/// address. Loaded at startup and rewritten whenever an entry changes.
#[derive(Default)]
pub struct Hosts {
    path: Option<PathBuf>,
    map: HashMap<Address, u32>,
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

    /// Record (and persist) the fingerprint `addr` is now bonded under.
    pub fn set(&mut self, addr: Address, fingerprint: u32) {
        if self.map.insert(addr, fingerprint) != Some(fingerprint) {
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
            .filter(|(_, fp)| **fp != current)
            .map(|(addr, _)| *addr)
            .collect();
        stale.sort_by_key(|a| a.0); // stable order for logs and menu markers
        stale
    }

    /// Rewrite the file. Failures are logged at debug and otherwise ignored —
    /// losing this state costs a stale marker, not correctness.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut text = String::new();
        for (addr, fp) in &self.map {
            text.push_str(&format!("{addr} {fp:08x}\n"));
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

/// Parse `<address> <hex fingerprint>` lines, skipping anything malformed.
fn parse(text: &str) -> HashMap<Address, u32> {
    text.lines()
        .filter_map(|line| {
            let (addr, fp) = line.split_once(' ')?;
            Some((
                addr.trim().parse().ok()?,
                u32::from_str_radix(fp.trim(), 16).ok()?,
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

    #[test]
    fn parses_and_skips_junk() {
        let map =
            parse("00:00:00:00:00:01 deadbeef\nnonsense\n\n00:00:00:00:00:02 0000000f\nzz ff\n");
        assert_eq!(map.len(), 2);
        assert_eq!(map[&addr(1)], 0xdead_beef);
        assert_eq!(map[&addr(2)], 0x0f);
    }

    #[test]
    fn stale_lists_only_mismatches() {
        let mut hosts = Hosts::default();
        hosts.map.insert(addr(1), 1);
        hosts.map.insert(addr(2), 2);
        let stale = hosts.stale(2);
        assert_eq!(stale, vec![addr(1)]);
        // A host with no record is unknown, not stale.
        assert!(!hosts.map.contains_key(&addr(3)));
        assert!(!hosts.stale(2).contains(&addr(3)));
    }

    #[test]
    fn forget_drops_the_record() {
        let mut hosts = Hosts::default();
        hosts.map.insert(addr(1), 7);
        hosts.forget(addr(1));
        assert!(!hosts.map.contains_key(&addr(1)));
    }
}
