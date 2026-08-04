"""What both guests have in common: a controller, a bus, a bluetoothd, btmon.

The controller is the interesting part. `btproxy -c <host> -p <port>` opens a TCP
connection to the host's `btvirt -t` and bridges it onto a local `/dev/vhci`
controller, which inside the guest is an ordinary `hci0`. Every client of that
one server lands in btvirt's single process-global `btdev_list`, which inquiry
and LE scan walk -- so *connecting two clients to one server is what makes the
two VMs able to see each other*, and there is no other wiring (design/TESTS.md
§2).

btvirt assigns addresses by client index, so connection order is each VM's
identity (§2.3). The hub serialises it; nothing here may connect on its own
initiative.
"""

import os
import re
import signal
import subprocess
import time

from common import (
    BluezStack,
    HarnessError,
    Process,
    log,
    rundir,
    run_btmgmt,
    wait_for,
)

BTPROXY = os.environ.get("BTPROXY", "/usr/bin/btproxy")
BTMON = os.environ.get("BTMON", "/usr/bin/btmon")
RADIO_ADDR = os.environ.get("RADIO_ADDR", "10.0.2.2")
RADIO_PORT = os.environ.get("RADIO_PORT", "45550")

BLUETOOTH_STORAGE = "/var/lib/bluetooth"
ADDRESS_RE = re.compile(r"(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}")


class GuestStack(BluezStack):
    """btproxy -> hci0 -> dbus -> bluetoothd, inside one guest."""

    def __init__(self, disable_plugins):
        super().__init__(disable_plugins=disable_plugins)
        self.address = None
        self.btmon = None

    def start_radio(self):
        subprocess.run(["modprobe", "hci_vhci"], check=False,
                       capture_output=True)
        if not os.path.exists("/dev/vhci"):
            raise HarnessError("/dev/vhci absent -- hci_vhci did not load")
        if not os.access(BTPROXY, os.X_OK):
            raise HarnessError(f"btproxy not executable: {BTPROXY}")

        log(f"bridging {RADIO_ADDR}:{RADIO_PORT} onto /dev/vhci")
        self.spawn("btproxy", [BTPROXY, "-c", RADIO_ADDR, "-p", RADIO_PORT])
        wait_for(lambda: os.path.isdir("/sys/class/bluetooth/hci0"), 20.0,
                 "the btproxy controller to register as hci0")

        self.start_dbus()
        # bluetoothd adopts the controller and applies its own persisted
        # settings, clobbering anything set before it starts -- so controller
        # configuration comes after it is up, not before.
        self.start_bluetoothd()
        self.configure_adapter()
        return {"address": self.address,
                "settings": self.current_settings(0)}

    def configure_adapter(self):
        self.require_controller()
        for cmd in (("power", "on"), ("connectable", "on"),
                    ("bondable", "on"), ("pairable", "on"), ("ssp", "on")):
            run_btmgmt(0, *cmd)
        self.address = self.read_address(0)
        # mgmt settings are applied asynchronously, and a bluetoothd that has
        # only just adopted the controller is still writing its own -- so the
        # state is waited for rather than read once. `connectable` is what
        # enables page scan; without it the peer's dial page-times-out, three
        # tests later and for no visible reason.
        try:
            settings = wait_for(
                lambda: (self.current_settings(0)
                         if all(r in self.current_settings(0)
                                for r in ("powered", "connectable"))
                         else None),
                20.0, "hci0 to become powered and connectable", interval=0.5)
        except HarnessError:
            raise HarnessError(
                f"hci0 never became powered and connectable (settings: "
                f"{self.current_settings(0)}); the peer cannot reach it"
            ) from None
        log(f"hci0: {self.address}  [{settings}]")
        return self.address

    def quiesce(self, power_off=False):
        """Drop the links cleanly before bluetoothd is stopped.

        The emulated radio is not a chip: it is `btvirt` on the other side of a
        TCP connection, and `btproxy` feeds what arrives from it into
        `/dev/vhci`. If bluetoothd exits with a link up, the local HCI device
        closes while the peer is still transmitting, the next write into
        `/dev/vhci` fails, and btproxy tears the bridge down -- after which the
        controller is simply gone and every later step fails for reasons that
        look nothing like the cause ("Invalid Index", "bluetoothd never adopted
        hci0").

        Disconnecting first closes that race. **Powering off does not, and made
        it worse**: `vhci_write` refuses a packet whenever the HCI device is
        down, so a controller that is powered off but still bridged kills
        btproxy at the peer's very next transmission -- and on LE the peer
        transmits constantly, because blooter advertises. That is what took the
        bridge out under `restart_stack` and `wipe_bonds`; the power-off is
        therefore opt-in, for the one caller that genuinely needs it (changing
        the adapter's bearers, which the kernel only allows while down).
        """
        for peer in self.connections(0):
            run_btmgmt(0, "disconnect", peer)
        if power_off:
            run_btmgmt(0, "power", "off")
        # A moment for the peer to see the disconnection and stop sending; the
        # race this is closing is measured in packets, not seconds.
        time.sleep(1.0)

    def respawn_radio(self):
        """Rebuild the bridge after the controller has been lost.

        A safety net, not a fix: `quiesce` is what keeps it from happening. But
        the radio is a TCP connection to another process, it can still go, and
        one lost controller must not cost the remaining thirty tests -- which is
        exactly what it did before this existed.

        **The address changes.** btvirt hands out addresses by client index and
        the counter only goes up, so the guest comes back as a different
        machine; every bond referring to the old one is dead. The hub re-reads
        addresses after each reset for that reason, and the test during which
        this happened is lost either way.
        """
        log("the radio is gone; rebuilding the bridge")
        self.stop_bluetoothd()
        for proc in [p for p in self.procs if p.name == "btproxy"]:
            proc.stop()
            self.procs.remove(proc)
        self.spawn("btproxy", [BTPROXY, "-c", RADIO_ADDR, "-p", RADIO_PORT])
        wait_for(lambda: os.path.isdir("/sys/class/bluetooth/hci0"), 20.0,
                 "the rebuilt btproxy controller to register as hci0")
        self.start_bluetoothd(timeout=60.0)
        self.configure_adapter()
        return self.address

    def require_controller(self):
        """Say plainly when the emulated controller has gone away.

        The radio is a TCP connection to another process on another machine, so
        it can vanish in ways a real controller cannot -- and when it does, every
        later step fails with something unhelpful ("Invalid Index", "bluetoothd
        did not adopt hci0") a long way from the cause. Naming it here, with
        btproxy's state, turns a cascade into one line.
        """
        if os.path.isdir("/sys/class/bluetooth/hci0"):
            return
        proxy = next((p for p in self.procs if p.name == "btproxy"), None)
        state = ("btproxy is gone" if proxy is None
                 else "btproxy is still running" if proxy.alive()
                 else f"btproxy exited (rc={proxy.proc.returncode}): "
                      + proxy.output().strip()[-300:])
        raise HarnessError(
            f"hci0 has disappeared -- the emulated radio is no longer bridged "
            f"into this guest ({state})")

    def set_transport(self, protocol):
        """Present the adapter as the kind of device blooter is configured to be.

        Without this a `protocol = "ble"` run is not really a BLE run. blooter
        stops registering the Classic HID profile, but the *adapter* keeps its
        BR/EDR page scan and its SDP records, so a dual-mode host discovers both
        bearers, merges them into one device object, and `Connect()` picks
        BR/EDR -- where it finds no HID record at all. (Observed exactly that: a
        successful BR/EDR connection carrying AVRCP and PnP and nothing else,
        while the LE advertisement went untouched.)

        A HOGP peripheral is an LE device, so the adapter is made one. This is
        environment, not behaviour -- blooter's code is untouched, and it is the
        same kind of setup choice as `tests/btvirt` bonding its controllers up
        front.

        `bredr`/`le` are only settable while powered down, so this power-cycles
        the adapter and has to happen before blooter starts. It is skipped when
        the adapter is already the right kind of device: the emulated controller
        is a TCP connection to another process, and power-cycling it dozens of
        times a run has been seen to lose it altogether. The suite also runs all
        the Classic scenarios before all the BLE ones, so in practice this fires
        about twice.
        """
        self.require_controller()
        wanted_bredr = protocol != "ble"
        if ("br/edr" in self.current_settings(0)) == wanted_bredr:
            return self.current_settings(0)

        # Same care as a bluetoothd restart, for the same reason: powering the
        # adapter down under a live link loses the bridge (see `quiesce`). This
        # is the one caller that has to power off at all -- the bearers are only
        # settable while the controller is down.
        self.quiesce(power_off=True)
        if protocol == "ble":
            run_btmgmt(0, "bredr", "off")
            run_btmgmt(0, "le", "on")
        else:
            run_btmgmt(0, "bredr", "on")
        self.configure_adapter()
        return self.current_settings(0)

    def restart_bluetoothd(self):
        """Stop and restart bluetoothd over the *same* bond storage.

        This is what stands in for a host reboot (design/TESTS.md J4): vng runs
        a guest for exactly one command, so a real reboot would end the VM. What
        J4 is actually asking -- does the bond survive on both sides, and does
        input work once the host stack comes back -- is fully exercised by
        restarting the daemon onto the bonds it persisted, since that persisted
        state is the only thing a reboot would have carried across. What it does
        *not* cover is anything the kernel forgets, which is stated as a gap in
        this suite's README.
        """
        self.quiesce()
        self.restart_daemon()
        return self.address

    def restart_daemon(self, wipe=False):
        """Stop bluetoothd, optionally wipe its storage, and start it again.

        Shared by the restart and the wipe because they differ by exactly one
        line -- and because the failure they share is worth handling in one
        place: if the bridge went down with the daemon, waiting 60 s for a
        controller that no longer exists reports "bluetoothd never adopted
        hci0", which says nothing about what happened. Rebuilding the bridge
        here keeps the *next* test running (its address changes, so this one is
        lost either way) and names the cause while it is still legible.
        """
        # SIGKILL, not SIGTERM, and this is the whole reason the restart used to
        # take the radio with it: a bluetoothd that exits cleanly powers down
        # the adapters it powered up, and `vhci_write` refuses every packet
        # while the controller is down -- so the peer's next transmission (on
        # LE, its next advertisement, milliseconds away) makes btproxy fail its
        # write and tear the bridge down. Killed, bluetoothd never runs that
        # path, the controller stays up, and the bridge lives. It is also the
        # closer model of what this stands in for: a host that reboots does not
        # ask its daemon nicely either, and every bond is already on disk,
        # written when it was made.
        self.stop_bluetoothd(sig=signal.SIGKILL)
        if wipe:
            self.wipe_storage()
        if not os.path.isdir("/sys/class/bluetooth/hci0"):
            self.respawn_radio()
            raise HarnessError(
                "the emulated radio went down with bluetoothd; the bridge has "
                f"been rebuilt (this guest is now {self.address}), so the rest "
                "of the run continues, but this test is a casualty")
        # Generous: the replacement has to wait for the outgoing instance's
        # `org.bluez` bus name to be released before it can adopt anything, and
        # a restart that is merely slow must not read as one that failed.
        self.start_bluetoothd(timeout=60.0)
        self.configure_adapter()
        return self.address

    # -- btmon --------------------------------------------------------------

    def btmon_start(self, tag):
        """A per-test HCI capture.

        design/TESTS.md §7.5: two VMs make a failure twice as hard to read, and
        from experience the one thing that reliably explains a pairing failure is
        btmon -- the `AuthenticationFailed` root cause was invisible in every log
        and obvious in one line of it.
        """
        self.btmon_stop()
        if not os.access(BTMON, os.X_OK):
            return None
        self.btmon = Process(f"btmon-{tag}", [BTMON, "-i", "0", "-t"])
        return self.btmon.log_path

    def btmon_stop(self):
        if self.btmon is not None:
            self.btmon.stop()
            self.btmon = None

    def btmon_output(self, tail=200):
        if self.btmon is None:
            return ""
        return "\n".join(self.btmon.output().splitlines()[-tail:])


class RoleAgent:
    """Commands both roles answer. `dev.py` and `host.py` add their own."""

    role = "?"
    disable_plugins = ()
    # Kernel modules this role needs loaded before anything else. virtme-ng
    # boots the host kernel with nothing loaded, and neither of these is
    # autoloaded on first open -- a missing one shows up much later as
    # "Unable to create UHID" three log levels down in bluetoothd, which is a
    # long way from "modprobe".
    modules = ()

    def __init__(self):
        self.stack = GuestStack(self.disable_plugins)
        self.commands = {
            "ping": self.ping,
            "start_radio": self.start_radio,
            "address": self.address,
            "settings": self.settings,
            "btmgmt": self.btmgmt,
            "shell": self.shell,
            "logs": self.logs,
            "restart_bluetoothd": self.restart_bluetoothd,
            "wipe_bonds": self.wipe_bonds,
            "unbond_all": self.unbond_all,
            "ensure_radio": self.ensure_radio,
            "bonded": self.bonded,
            "connections": self.connections,
            "btmon_start": self.btmon_start,
            "btmon_stop": self.btmon_stop,
            "btmon_output": self.btmon_output,
        }

    # -- lifecycle ----------------------------------------------------------

    def ping(self):
        return {"role": self.role, "rundir": rundir(),
                "kernel": os.uname().release}

    def start_radio(self):
        for module in self.modules:
            subprocess.run(["modprobe", module], check=False,
                           capture_output=True)
        missing = [m for m in self.modules
                   if not os.path.exists(f"/dev/{m}")]
        if missing:
            raise HarnessError(
                f"{self.role}: /dev/{', /dev/'.join(missing)} absent after "
                "modprobe; this kernel cannot run the suite")
        return self.stack.start_radio()

    def address(self):
        return self.stack.address

    def ensure_radio(self):
        """Return this guest's address, rebuilding the bridge if it has been
        lost. Called before every test, so one casualty is one test."""
        if not os.path.isdir("/sys/class/bluetooth/hci0"):
            return self.stack.respawn_radio()
        return self.stack.address

    def settings(self):
        return self.stack.current_settings(0)

    def restart_bluetoothd(self):
        return self.stack.restart_bluetoothd()

    def unbond_all(self):
        """Drop every bond the ordinary way, through bluetoothd.

        This is how each test is returned to unpaired adapters. It deliberately
        does *not* restart anything: a `bluetoothd` restart between every one of
        thirty-odd tests is both slow and a source of failures that have nothing
        to do with what is being tested. `wipe_bonds` is the heavier hammer, and
        it is reserved for D6, which is *about* the storage going away.
        """
        for address in list(self.bonded()):
            subprocess.run(["bluetoothctl", "remove", address],
                           capture_output=True, stdin=subprocess.DEVNULL,
                           timeout=30.0, check=False)
        # Anything bluetoothd would not let go of, take at the mgmt layer. Both
        # transports have to be named: `unpair` defaults to BR/EDR, so an LE
        # bond survives a plain one.
        for address in list(self.bonded()):
            for addr_type in ("0", "1"):
                run_btmgmt(0, "unpair", "-t", addr_type, address)
        remaining = self.bonded()
        if remaining:
            log(f"WARNING: bonds survived removal ({remaining}); wiping storage")
            return self.wipe_bonds()
        return remaining

    def wipe_bonds(self):
        """Delete every stored bond and restart bluetoothd onto the empty store
        (design/TESTS.md D6 -- what a reinstall looks like).

        The *kernel* has to be told too. bluetoothd's storage is only half of
        where a bond lives; the link keys and LTKs it loaded are in the
        controller's key list, and a reinstall would come back to a kernel that
        never had them. Without this the wiped side happily accepts an encrypted
        reconnect from a host it no longer has any record of -- which reads as
        D6 failing to diverge, and is really the harness leaving half the bond
        in place.
        """
        addresses = list(self.bonded())
        self.stack.quiesce()
        self.stack.stop_bluetoothd(sig=signal.SIGKILL)
        for address in addresses:
            for addr_type in ("0", "1"):
                run_btmgmt(0, "unpair", "-t", addr_type, address)
        self.stack.restart_daemon(wipe=True)
        return self.bonded()

    # -- queries ------------------------------------------------------------

    def bonded(self):
        """This side's bond store, read straight off disk.

        `{address: {"linkkey": bool, "ltk": bool}}` from
        `/var/lib/bluetooth/<adapter>/<peer>/info`. Deliberately *not* from
        bluetoothctl: the premise of the whole suite is that the two stores can
        disagree, so each side is asked what it actually holds rather than what
        it believes about the peer. Reading the keys rather than a `Paired:`
        flag is also what makes the transport visible -- a `[LinkKey]` is a
        BR/EDR bond and a `[LongTermKey]` an LE one, which is the distinction
        D7 turns on.
        """
        out = {}
        for adapter in _subdirs(BLUETOOTH_STORAGE):
            for peer in _subdirs(os.path.join(BLUETOOTH_STORAGE, adapter)):
                if not ADDRESS_RE.fullmatch(peer):
                    continue  # "cache", "settings", ...
                info = _read(os.path.join(
                    BLUETOOTH_STORAGE, adapter, peer, "info"))
                if info is None:
                    continue
                out[peer.upper()] = {
                    "linkkey": "[LinkKey]" in info,
                    "ltk": "[LongTermKey]" in info or "[SlaveLongTermKey]" in info,
                }
        return out

    def connections(self):
        return self.stack.connections(0)

    def btmgmt(self, args, timeout=30.0):
        return run_btmgmt(0, *args, timeout=timeout)

    def shell(self, argv, timeout=30.0, check=False):
        result = subprocess.run(argv, capture_output=True, text=True,
                                stdin=subprocess.DEVNULL, timeout=timeout,
                                check=check)
        return {"rc": result.returncode,
                "out": result.stdout + result.stderr}

    def logs(self, tail=200):
        return self.stack.logs(tail=tail)

    # -- btmon --------------------------------------------------------------

    def btmon_start(self, tag):
        return self.stack.btmon_start(tag)

    def btmon_stop(self):
        self.stack.btmon_stop()

    def btmon_output(self, tail=200):
        return self.stack.btmon_output(tail=tail)

    # -- teardown -----------------------------------------------------------

    def shutdown(self):
        self.stack.btmon_stop()
        self.stack.stop()


def _subdirs(path):
    try:
        return [e for e in os.listdir(path)
                if os.path.isdir(os.path.join(path, e))]
    except OSError:
        return []


def _read(path):
    try:
        with open(path, "r") as fh:
            return fh.read()
    except OSError:
        return None
