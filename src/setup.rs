//! Adapter preparation: present blooter as a keyboard/mouse peripheral
//! (class 0x05/0x40) named "blooter" with Simple Secure Pairing enabled, so
//! hosts recognise and pair with us easily. The class, name and SSP mode go
//! through the BlueZ management socket and are restored on drop; the adapter
//! *alias* goes over D-Bus and is restored explicitly (see [`Identity`]).
//!
//! Both halves matter on BLE, which is why this module is no longer
//! Classic-only. bluetoothd owns the GAP service (0x1800) and serves its Device
//! Name from the adapter alias and its Appearance from the adapter's Class of
//! Device, so a connected host reads *these* values — not the name and
//! appearance in the LE advertisement, which only reach it beforehand
//! (design/CONNECTION.md §4.1). How far this goes is `[ble] advertise`.
//!
//! The interactive host (re)connection menu lives in [`crate::menu`].

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;

use bluer::Adapter;
use log::{debug, info, warn};

use crate::config::Advertise;

const BTPROTO_HCI: libc::c_int = 1;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_CONTROL: u16 = 3;

const MGMT_OP_READ_INFO: u16 = 0x0004;
const MGMT_OP_SET_SSP: u16 = 0x000c;
const MGMT_OP_SET_DEV_CLASS: u16 = 0x000e;
const MGMT_OP_SET_LOCAL_NAME: u16 = 0x000f;
const MGMT_EV_CMD_COMPLETE: u16 = 0x0001;
const MGMT_EV_CMD_STATUS: u16 = 0x0002;
const MGMT_SETTING_SSP: u32 = 1 << 6;

/// Class of Device advertised while running: peripheral major, keyboard minor.
const CLASS: [u8; 2] = [0x05, 0x40];
const NAME: &[u8] = b"blooter";
/// Length of the Set Local Name parameters: name[249] + short_name[11].
const NAME_LEN: usize = 260;
/// Length of the Read Controller Information reply.
const INFO_LEN: usize = 20 + NAME_LEN;

#[repr(C)]
struct SockaddrHci {
    hci_family: libc::sa_family_t,
    hci_dev: u16,
    hci_channel: u16,
}

/// A blocking BlueZ management-API socket bound to one controller index.
struct Mgmt {
    sock: File,
    index: u16,
}

impl Mgmt {
    fn open(index: u16) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                BTPROTO_HCI,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sock = unsafe { File::from_raw_fd(fd) };
        let sa = SockaddrHci {
            hci_family: libc::AF_BLUETOOTH as libc::sa_family_t,
            hci_dev: HCI_DEV_NONE,
            hci_channel: HCI_CHANNEL_CONTROL,
        };
        let rc = unsafe {
            libc::bind(
                fd,
                &sa as *const SockaddrHci as *const libc::sockaddr,
                size_of::<SockaddrHci>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { sock, index })
    }

    /// Send one command and wait for its Command Complete/Status, skipping
    /// unrelated events. Returns the reply parameters.
    fn command(&mut self, opcode: u16, params: &[u8]) -> io::Result<Vec<u8>> {
        let mut pkt = Vec::with_capacity(6 + params.len());
        pkt.extend_from_slice(&opcode.to_le_bytes());
        pkt.extend_from_slice(&self.index.to_le_bytes());
        pkt.extend_from_slice(&(params.len() as u16).to_le_bytes());
        pkt.extend_from_slice(params);
        self.sock.write_all(&pkt)?;

        let mut buf = [0u8; 1024];
        loop {
            let n = self.sock.read(&mut buf)?;
            let Some(payload) = buf.get(6..n) else {
                continue;
            };
            let event = u16::from_le_bytes([buf[0], buf[1]]);
            if (event != MGMT_EV_CMD_COMPLETE && event != MGMT_EV_CMD_STATUS)
                || payload.len() < 3
                || u16::from_le_bytes([payload[0], payload[1]]) != opcode
            {
                continue;
            }
            let status = payload[2];
            if status != 0 {
                return Err(io::Error::other(format!(
                    "mgmt command 0x{opcode:04x} failed: status 0x{status:02x}"
                )));
            }
            return Ok(payload[3..].to_vec());
        }
    }
}

/// The adapter state saved at startup; restores it when dropped.
pub struct BtSetup {
    mgmt: Mgmt,
    class: [u8; 3],
    name: [u8; NAME_LEN],
    had_ssp: bool,
}

/// Save the current class/name/SSP mode of controller `hci<index>`, then set
/// class 0x05/0x40, name "blooter" and SSP on.
pub fn apply(index: u16) -> io::Result<BtSetup> {
    let mut mgmt = Mgmt::open(index)?;
    let info = mgmt.command(MGMT_OP_READ_INFO, &[])?;
    if info.len() < INFO_LEN {
        return Err(io::Error::other("short mgmt controller info"));
    }
    let current = u32::from_le_bytes(info[13..17].try_into().unwrap());
    let mut setup = BtSetup {
        class: info[17..20].try_into().unwrap(),
        name: info[20..INFO_LEN].try_into().unwrap(),
        had_ssp: current & MGMT_SETTING_SSP != 0,
        mgmt,
    };

    setup.mgmt.command(MGMT_OP_SET_DEV_CLASS, &CLASS)?;
    let mut name = [0u8; NAME_LEN];
    name[..NAME.len()].copy_from_slice(NAME);
    name[249..249 + NAME.len()].copy_from_slice(NAME);
    setup.mgmt.command(MGMT_OP_SET_LOCAL_NAME, &name)?;
    if !setup.had_ssp {
        setup.mgmt.command(MGMT_OP_SET_SSP, &[1])?;
    }
    info!(
        "adapter hci{index}: class 0x{:02x}{:02x}, name \"blooter\", SSP on",
        CLASS[0], CLASS[1]
    );
    Ok(setup)
}

/// The controller index behind a bluer adapter, i.e. the `N` of `hciN`, which
/// is what the management socket is addressed by.
pub fn adapter_index(adapter: &Adapter) -> u16 {
    adapter
        .name()
        .strip_prefix("hci")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// The adapter identity blooter took over, and what to put back on exit.
///
/// The mgmt-set class/name/SSP restore themselves when the [`BtSetup`] guard
/// drops. The alias and the discoverable flag are D-Bus properties, so undoing
/// them needs an await and cannot happen in `Drop`: call [`Identity::restore`].
pub struct Identity {
    adapter: Adapter,
    /// The alias to put back: `Some(name)` for one that was explicitly set,
    /// `None` when there was none and the adapter should fall back to the
    /// system name again. Absent when the alias was never changed.
    alias: Option<Option<String>>,
    /// The discoverable flag to put back, when it was changed.
    discoverable: Option<bool>,
}

impl Identity {
    /// Put back everything this guard changed. Best-effort: a failure here is
    /// worth a warning but never changes the exit status.
    pub async fn restore(self) {
        if let Some(prior) = self.alias {
            // BlueZ treats an empty alias as "drop the stored alias and go back
            // to the system name", so restoring "no alias" must not write the
            // hostname back as an alias — that would pin it.
            let value = prior.unwrap_or_default();
            if let Err(e) = self.adapter.set_alias(value).await {
                warn!("cannot restore adapter alias: {e}");
            }
        }
        if let Some(prior) = self.discoverable
            && let Err(e) = self.adapter.set_discoverable(prior).await
        {
            warn!("cannot restore adapter discoverable state: {e}");
        }
    }
}

/// Set the adapter alias to "blooter", so a connected host reads that as the
/// GAP Device Name rather than the machine's hostname. Returns what to restore.
///
/// Applies to both transports: on Classic the alias is what BlueZ puts in the
/// EIR, and the mgmt local name set by [`apply`] does not update it.
pub async fn take_alias(adapter: &Adapter) -> Identity {
    let name = String::from_utf8_lossy(NAME).into_owned();
    let current = adapter.alias().await.ok();
    // An adapter with no alias of its own reports the system name here, and
    // that is exactly the case where the restore must clear the alias rather
    // than write the reported value back.
    let system = adapter.system_name().await.ok();
    let alias = match adapter.set_alias(name.clone()).await {
        Ok(()) => {
            info!("adapter alias set to \"{name}\"");
            Some(current.filter(|c| Some(c) != system.as_ref()))
        }
        Err(e) => {
            warn!("cannot set adapter alias, so hosts will see the system name: {e}");
            None
        }
    };
    Identity {
        adapter: adapter.clone(),
        alias,
        discoverable: None,
    }
}

/// Apply the `[ble] advertise` policy to `adapter` (design/CONNECTION.md §4.1).
///
/// Returns the mgmt guard (when the Class of Device was taken over) and the
/// identity to restore on exit. `AliasCod` is the mode that insists: it reports
/// an error rather than quietly presenting the wrong device icon, which is what
/// `Auto` does deliberately since the class needs `CAP_NET_ADMIN`.
pub async fn apply_ble_identity(
    adapter: &Adapter,
    mode: Advertise,
) -> Result<(Option<BtSetup>, Identity), String> {
    let mut identity = take_alias(adapter).await;

    let want_class = !matches!(mode, Advertise::Alias);
    let bt = match want_class.then(|| apply(adapter_index(adapter))) {
        None => None,
        Some(Ok(s)) => Some(s),
        // `Auto` is "do what you can": a class needs CAP_NET_ADMIN, and losing
        // the keyboard icon is not worth a warning the user cannot act on.
        Some(Err(e)) if mode == Advertise::Auto => {
            debug!("cannot set the adapter class ({e}); advertising with the alias only");
            None
        }
        Some(Err(e)) => {
            return Err(format!(
                "[ble] advertise = \"{}\" cannot set the adapter class: {e} \
                 (needs CAP_NET_ADMIN; use \"auto\" to carry on without it)",
                if mode == Advertise::AliasCod {
                    "alias_cod"
                } else {
                    "alias_cod_hide"
                }
            ));
        }
    };

    if mode == Advertise::AliasCodHide {
        // BlueZ has no per-transport discoverable flag, so this is as far as
        // hiding goes: the LE advertisement stays up (it is its own channel),
        // while BR/EDR inquiry no longer answers.
        let was = adapter.is_discoverable().await.unwrap_or(false);
        match adapter.set_discoverable(false).await {
            Ok(()) => {
                info!("BR/EDR discoverability off; only the LE identity is on offer");
                if was {
                    identity.discoverable = Some(true);
                }
            }
            Err(e) => warn!("cannot turn off BR/EDR discoverability: {e}"),
        }
    }

    Ok((bt, identity))
}

impl Drop for BtSetup {
    fn drop(&mut self) {
        if !self.had_ssp
            && let Err(e) = self.mgmt.command(MGMT_OP_SET_SSP, &[0])
        {
            warn!("cannot restore SSP mode: {e}");
        }
        // Class bytes are little-endian CoD: [0] = minor, [1] = major | service.
        let class = [self.class[1] & 0x1f, self.class[0]];
        if let Err(e) = self.mgmt.command(MGMT_OP_SET_DEV_CLASS, &class) {
            warn!("cannot restore device class: {e}");
        }
        if let Err(e) = self.mgmt.command(MGMT_OP_SET_LOCAL_NAME, &self.name) {
            warn!("cannot restore adapter name: {e}");
        }
        info!("adapter settings restored");
    }
}
