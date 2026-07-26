//! Adapter preparation via the BlueZ management socket: advertise as a
//! keyboard/mouse peripheral (class 0x05/0x40) named "blooter" with Simple
//! Secure Pairing enabled, so hosts recognise and pair with us easily. The
//! original class, name and SSP mode are restored on drop. The interactive host
//! (re)connection menu lives in [`crate::menu`].

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;

use log::{info, warn};

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
