"""A private system bus and a bluetoothd on it.

Both suites need exactly this and need it to be the *same* thing: a throwaway
`dbus-daemon --system`, a tmpfs over `/var/lib/bluetooth` (virtme-ng exports the
host filesystem read-only, and it also means no bond outlives the run), and a
bluetoothd started with `--nodetach -d`. What differs between the suites is only
which plugins are disabled and where the controllers come from, so both are
parameters.
"""

import os
import re
import signal
import subprocess

from .process import HarnessError, Process, log, rundir, wait_for

BLUETOOTHD = os.environ.get(
    "BLUETOOTHD", "/usr/libexec/bluetooth/bluetoothd")
DBUS_DAEMON = "/usr/bin/dbus-daemon"

BUS_SOCKET = "/run/dbus/system_bus_socket"
BUS_ADDRESS = f"unix:path={BUS_SOCKET}"

BLUETOOTH_STORAGE = "/var/lib/bluetooth"


def btmgmt_path():
    # Built alongside btvirt: see tests/btvirt/build-btvirt.sh for why the
    # packaged one is not good enough.
    return os.environ.get("BTMGMT", "/usr/bin/btmgmt")


def run_btmgmt(index, *args, timeout=30.0):
    """Run a btmgmt subcommand and return its combined output.

    stdin is /dev/null and a timeout is always set: btmgmt drops into an
    interactive prompt for anything it does not recognise as a one-shot command
    (`paired`, for one), which would otherwise hang the whole suite.
    """
    argv = [btmgmt_path(), "--index", str(index)] + [str(a) for a in args]
    try:
        result = subprocess.run(argv, capture_output=True, text=True,
                                stdin=subprocess.DEVNULL, timeout=timeout,
                                check=False)
    except subprocess.TimeoutExpired:
        # Silence here once cost an afternoon: every call timing out looks like
        # an empty reply, three levels down.
        log(f"btmgmt {' '.join(argv[3:])} timed out after {timeout}s")
        return ""
    return result.stdout + result.stderr


class BluezStack:
    """dbus-daemon + bluetoothd, and the mgmt queries that go with them.

    Subclasses supply the controllers: `tests/btvirt` runs `btvirt -l2` in the
    same kernel, `tests/twovm` bridges one in from the host's `btvirt -t` with
    `btproxy`.
    """

    def __init__(self, disable_plugins=("input",), name="bluetoothd"):
        self.disable_plugins = tuple(disable_plugins)
        self.bluetoothd_name = name
        self.procs = []
        self.bluetoothd = None

    # -- process bookkeeping ------------------------------------------------

    def spawn(self, name, argv, env=None):
        proc = Process(name, argv, env=env)
        self.procs.append(proc)
        return proc

    # -- dbus ---------------------------------------------------------------

    def start_dbus(self):
        """A private system bus: the guest has no running one, and this keeps
        the test's bluetoothd off any bus the host might share in."""
        for path in ("/run/dbus", "/var/run/dbus"):
            os.makedirs(path, exist_ok=True)
        self.mount_storage()
        log("starting private system bus")
        self.spawn("dbus", [DBUS_DAEMON, "--system", "--nofork", "--nopidfile"])
        wait_for(lambda: os.path.exists(BUS_SOCKET), 10.0,
                 "the system bus socket")
        os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = BUS_ADDRESS

    def mount_storage(self):
        """virtme-ng exports the host filesystem read-only, so bluetoothd cannot
        write its adapter state. Give it a throwaway tmpfs -- which also means
        bonds never outlive the run."""
        if os.path.ismount(BLUETOOTH_STORAGE):
            return
        os.makedirs(BLUETOOTH_STORAGE, exist_ok=True)
        subprocess.run(["mount", "-t", "tmpfs", "tmpfs", BLUETOOTH_STORAGE],
                       capture_output=True, check=False)

    def wipe_storage(self):
        """Delete every stored bond, as a reinstall would (design/TESTS.md D6).

        The mount stays; only its contents go, so bluetoothd can be restarted
        onto the same empty directory.
        """
        for entry in os.listdir(BLUETOOTH_STORAGE):
            subprocess.run(["rm", "-rf", os.path.join(BLUETOOTH_STORAGE, entry)],
                           check=False)

    # -- bluetoothd ---------------------------------------------------------

    def start_bluetoothd(self, adopt="hci0", timeout=30.0):
        """Start bluetoothd and wait until it has adopted a controller.

        `disable_plugins` is what keeps blooter's own HID profile registrable:
        `-P input` drops the built-in input plugin, which would otherwise own
        the HID UUID (see the error text in main.rs::register_profile). The host
        side of `tests/twovm` deliberately keeps it -- there, the kernel HID
        stack driven by that plugin is the thing under test.
        """
        argv = [BLUETOOTHD, "--nodetach", "-d"]
        for plugin in self.disable_plugins:
            argv += ["-P", plugin]
        log(f"starting bluetoothd ({' '.join(argv[1:])})")
        bt = self.spawn(self.bluetoothd_name, argv)
        self.bluetoothd = bt
        bt.wait_for_output(r"Bluetooth management interface|Starting SDP server",
                           timeout, "bluetoothd to come up")
        if adopt:
            # bluetoothd re-powers controllers on adoption; wait until it has
            # taken the controller, otherwise blooter may race it for the
            # default adapter.
            bt.wait_for_output(adopt, timeout,
                               f"bluetoothd to adopt {adopt}")
        return bt

    def stop_bluetoothd(self, sig=signal.SIGTERM):
        if self.bluetoothd is not None:
            self.bluetoothd.stop(sig=sig)
            if self.bluetoothd in self.procs:
                self.procs.remove(self.bluetoothd)
            self.bluetoothd = None

    # -- mgmt queries -------------------------------------------------------

    def read_address(self, index):
        """btvirt's controllers keep their own fixed BD_ADDRs, ignore
        `public-addr` and expose no sysfs `address` node -- so the address is
        read back from mgmt, never assigned."""
        out = run_btmgmt(index, "info")
        match = re.search(r"addr ([0-9A-Fa-f:]{17})", out)
        if not match:
            raise HarnessError(f"cannot read hci{index} address from:\n{out}")
        addr = match.group(1).upper()
        if addr == "00:00:00:00:00:00":
            raise HarnessError(f"hci{index} has no usable address")
        return addr

    def current_settings(self, index):
        out = run_btmgmt(index, "info")
        match = re.search(r"current settings:\s*(.*)", out)
        return match.group(1).strip() if match else ""

    def connections(self, index):
        out = run_btmgmt(index, "con")
        return re.findall(r"([0-9A-Fa-f:]{17})", out)

    # -- teardown / diagnostics ---------------------------------------------

    def stop(self):
        for proc in reversed(self.procs):
            proc.stop()
        self.procs = []
        self.bluetoothd = None

    def dump_logs(self, tail=40):
        for proc in self.procs:
            out = proc.output().strip()
            if out:
                print(f"\n--- {proc.name} ({proc.log_path}) ---", flush=True)
                print("\n".join(out.splitlines()[-tail:]), flush=True)

    def logs(self, tail=200):
        """Every component log, as one string -- what a remote caller wants when
        a test on the other side of a socket has failed."""
        chunks = []
        for proc in self.procs:
            out = proc.output().strip()
            if out:
                chunks.append(f"--- {proc.name} ({proc.log_path}) ---\n"
                              + "\n".join(out.splitlines()[-tail:]))
        chunks.append(f"(run directory: {rundir()})")
        return "\n\n".join(chunks)
