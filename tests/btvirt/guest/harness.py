"""End-to-end test harness for blooter's connection logic.

Runs inside the virtme-ng guest as root. Brings up the full stack --

    btvirt (2 emulated controllers) -> bluetoothd -> blooter

-- and drives it from the other controller with a fake HID host that speaks
real L2CAP on the standard HID PSMs. Nothing here is mocked: blooter is the
real binary, talking to a real bluetoothd over a real system bus, and the
reports asserted on are the bytes that actually crossed the L2CAP interrupt
channel.

hci0 plays blooter; hci1 plays the host that connects to it.

See ../README.md for why a VM (and not a container) is required.
"""

import os
import re
import signal
import socket
import struct
import subprocess
import sys
import time
import traceback

AF_BLUETOOTH = 31
BTPROTO_L2CAP = 0

CONTROL_PSM = 0x11
INTERRUPT_PSM = 0x13

# HIDP report headers (report.rs).
HIDP_DATA_INPUT = 0xA1
REPORT_ID_MOUSE = 0x01
REPORT_ID_KEYBOARD = 0x02

# HID_CONTROL | VIRTUAL_CABLE_UNPLUG -- "forget this device" (classic.rs).
VIRTUAL_CABLE_UNPLUG = 0x15

# setsockopt(SOL_BLUETOOTH, BT_SECURITY) -- see FakeHost._socket.
SOL_BLUETOOTH = 274
BT_SECURITY = 4
BT_SECURITY_LOW = 1

# evdev event types/codes (linux/input-event-codes.h).
EV_SYN = 0x00
EV_KEY = 0x01
EV_REL = 0x02
# SYN_REPORT: end of an input frame. blooter only emits a report at a frame
# boundary (design/ARCH.md §7.2c), so every injected event needs one after it.
SYN_REPORT = 0x00
KEY_A = 30
KEY_B = 48
BTN_LEFT = 0x110
REL_X = 0x00
REL_Y = 0x01

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))))
RUNDIR = "/tmp/blooter-btvirt"

BLUETOOTHD = "/usr/libexec/bluetooth/bluetoothd"
# Built alongside btvirt: see ../build-btvirt.sh for why the packaged one is
# not good enough.
BTMGMT = os.environ.get("BTMGMT", "/usr/bin/btmgmt")
DBUS_DAEMON = "/usr/bin/dbus-daemon"


class HarnessError(Exception):
    """Setup failed -- distinct from a test assertion failing."""


def log(msg):
    print(f"    | {msg}", flush=True)


def btmgmt(index, *args, timeout=30.0):
    """Run a btmgmt subcommand and return its combined output.

    stdin is /dev/null and a timeout is always set: btmgmt drops into an
    interactive prompt for anything it does not recognise as a one-shot command
    (`paired`, for one), which would otherwise hang the whole suite.
    """
    argv = [BTMGMT, "--index", str(index)] + [str(a) for a in args]
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


def wait_for(predicate, timeout, what, interval=0.05):
    """Poll until `predicate()` is truthy. Everything in this harness is
    asynchronous startup, so nothing gets a fixed sleep."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise HarnessError(f"timed out after {timeout}s waiting for {what}")


# --------------------------------------------------------------------------
# input_event synthesis
# --------------------------------------------------------------------------

# Native `struct input_event`: timeval(16) + type(2) + code(2) + value(4) = 24
# bytes on 64-bit, which is what input.rs::spawn_fifo parses.
_INPUT_EVENT = struct.Struct("@llHHi")
assert _INPUT_EVENT.size == 24, f"unexpected input_event size {_INPUT_EVENT.size}"


def input_event(type_, code, value):
    return _INPUT_EVENT.pack(0, 0, type_, code, value)


# --------------------------------------------------------------------------
# Stack components
# --------------------------------------------------------------------------

class Process:
    """A child process with its output captured to a file, so a failure can
    show what the component actually said."""

    def __init__(self, name, argv, env=None, stdin_pipe=False):
        self.name = name
        self.argv = argv
        self.log_path = os.path.join(RUNDIR, f"{name}.log")
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            argv,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            stdin=subprocess.PIPE if stdin_pipe else subprocess.DEVNULL,
            env=env,
            start_new_session=True,
        )

    def write_stdin(self, text):
        """Feed a line to a process started with `stdin_pipe=True`."""
        if self.proc.stdin is None:
            raise HarnessError(f"{self.name} was not started with a stdin pipe")
        self.proc.stdin.write(text.encode())
        self.proc.stdin.flush()

    def output(self):
        try:
            with open(self.log_path, "rb") as fh:
                return fh.read().decode("utf-8", "replace")
        except OSError:
            return ""

    def alive(self):
        return self.proc.poll() is None

    def match_count(self, pattern):
        return len(re.findall(pattern, self.output()))

    def wait_for_output(self, pattern, timeout, what=None, after=0):
        """Wait until `pattern` has matched more than `after` times.

        `after` matters whenever the same line can be logged once per session:
        a plain "has it appeared?" check is satisfied by the *previous*
        session's line and returns immediately.
        """
        what = what or f"{self.name} output matching {pattern!r}"

        def check():
            if not self.alive():
                raise HarnessError(
                    f"{self.name} exited (rc={self.proc.returncode}) while waiting "
                    f"for {what}\n--- {self.name} output ---\n{self.output()}"
                )
            return self.match_count(pattern) > after

        return wait_for(check, timeout, what)

    def stop(self, sig=signal.SIGTERM, timeout=5.0):
        if self.proc.poll() is None:
            try:
                os.killpg(os.getpgid(self.proc.pid), sig)
            except (ProcessLookupError, PermissionError):
                try:
                    self.proc.send_signal(sig)
                except ProcessLookupError:
                    pass
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    self.proc.kill()
                self.proc.wait(timeout=timeout)
        try:
            self._log.close()
        except OSError:
            pass
        return self.proc.returncode


class Stack:
    """btvirt + dbus + bluetoothd. Started once and shared by every test; only
    blooter itself is restarted per test, which keeps the suite quick and means
    a test exercises a fresh accept loop against a warm stack."""

    def __init__(self, btvirt_path):
        self.btvirt_path = btvirt_path
        self.procs = []
        self.addresses = {}

    def _spawn(self, name, argv, env=None):
        proc = Process(name, argv, env=env)
        self.procs.append(proc)
        return proc

    def start(self):
        os.makedirs(RUNDIR, exist_ok=True)
        self._start_btvirt()
        self._start_dbus()
        # bluetoothd adopts the controllers and applies its own persisted
        # settings, clobbering anything set before it starts -- so controller
        # configuration has to come after it is up, not before.
        self._start_bluetoothd()
        self._configure_controllers()
        self.pair_controllers()

    # -- controllers --------------------------------------------------------

    def _start_btvirt(self):
        if not os.access(self.btvirt_path, os.X_OK):
            raise HarnessError(f"btvirt not executable: {self.btvirt_path}")
        subprocess.run(["modprobe", "hci_vhci"], check=False,
                       capture_output=True)
        if not os.path.exists("/dev/vhci"):
            raise HarnessError("/dev/vhci absent -- hci_vhci did not load")

        log("starting btvirt with two controllers")
        self._spawn("btvirt", [self.btvirt_path, "-l2"])
        wait_for(
            lambda: os.path.isdir("/sys/class/bluetooth/hci0")
            and os.path.isdir("/sys/class/bluetooth/hci1"),
            10.0,
            "two btvirt controllers to register",
        )

    def _configure_controllers(self):
        # btvirt's controllers keep their own fixed BD_ADDRs and ignore
        # `public-addr`, and expose no sysfs `address` node -- so the addresses
        # are read back from mgmt (see ../README.md).
        for index in (0, 1):
            for cmd in (("power", "on"), ("connectable", "on"),
                        ("bondable", "on"), ("pairable", "on"), ("ssp", "on")):
                btmgmt(index, *cmd)
            self.addresses[index] = self._read_address(index)

        if self.addresses[0] == self.addresses[1]:
            raise HarnessError("both controllers report the same address")

        # hci0 must page-scan or the host's dial gets EHOSTDOWN (page timeout).
        # `connectable` is what enables page scan, so confirm it actually stuck
        # rather than discovering it went missing three tests later.
        for index in (0, 1):
            settings = self._current_settings(index)
            log(f"hci{index}: {self.addresses[index]}  [{settings}]")
            for required in ("powered", "connectable"):
                if required not in settings:
                    raise HarnessError(
                        f"hci{index} is not {required} (settings: {settings}); "
                        "the host will not be able to reach it")

    def pair_controllers(self):
        """Bond the two controllers once, before blooter ever starts.

        Both ends of the link live on the same bluetoothd here, which a real
        deployment never does. Once blooter registers its pairing agent, that
        one agent is the default for *both* adapters -- so an unbonded connect
        raises simultaneous SSP requests on hci0 and hci1, the second gets
        "Device or resource busy", authentication fails, and the connect is
        refused with EACCES.

        Bonding up front sidesteps the shared-agent artifact entirely: with a
        link key already stored, connects need no SSP and never reach the agent.
        It is also the realistic state for these tests -- a HID host that
        reconnects to blooter is one that already paired with it. Pairing
        happens here, while no agent is registered, so bluetoothd completes
        Just Works on its own.

        Pairing blooter's *agent* (design/CONNECTION.md §5) is a separate
        concern and is not covered by this suite; see README.md.
        """
        log("bonding the controllers (before blooter's agent exists)")
        # -c 3 = NoInputNoOutput, which negotiates Just Works.
        out = btmgmt(1, "pair", "-c", "3", "-t", "0", self.addresses[0])
        if "Paired with" not in out:
            raise HarnessError(
                "could not bond the controllers; btmgmt pair said:\n"
                + out.strip())
        # A link key on each side is what lets later connects skip SSP.
        if "new_link_key" not in out:
            log("WARNING: pairing reported success but stored no link key")
        log("controllers bonded")

    def _current_settings(self, index):
        out = btmgmt(index, "info")
        match = re.search(r"current settings:\s*(.*)", out)
        return match.group(1).strip() if match else ""

    def _read_address(self, index):
        out = btmgmt(index, "info")
        match = re.search(r"addr ([0-9A-Fa-f:]{17})", out)
        if not match:
            raise HarnessError(f"cannot read hci{index} address from:\n{out}")
        addr = match.group(1).upper()
        if addr == "00:00:00:00:00:00":
            raise HarnessError(f"hci{index} has no usable address")
        return addr

    # -- dbus + bluetoothd --------------------------------------------------

    def _start_dbus(self):
        # A private system bus: the guest has no running one, and this keeps the
        # test's bluetoothd off any bus the host might share in.
        for path in ("/run/dbus", "/var/run/dbus"):
            os.makedirs(path, exist_ok=True)
        # virtme-ng exports the host filesystem read-only, so bluetoothd cannot
        # write its adapter state. Give it a throwaway tmpfs -- which also means
        # bonds never outlive the run.
        subprocess.run(["mount", "-t", "tmpfs", "tmpfs", "/var/lib/bluetooth"],
                       capture_output=True, check=False)
        log("starting private system bus")
        self._spawn("dbus", [DBUS_DAEMON, "--system", "--nofork", "--nopidfile"])
        wait_for(lambda: os.path.exists("/run/dbus/system_bus_socket"),
                 10.0, "the system bus socket")
        os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = \
            "unix:path=/run/dbus/system_bus_socket"

    def _start_bluetoothd(self):
        # `-P input` disables the built-in input plugin, which would otherwise
        # own the HID UUID and make blooter's profile registration fail (see the
        # error text in main.rs::register_profile).
        log("starting bluetoothd (-P input)")
        bt = self._spawn("bluetoothd", [BLUETOOTHD, "--nodetach", "-P", "input", "-d"])
        bt.wait_for_output(r"Bluetooth management interface|Starting SDP server",
                           15.0, "bluetoothd to come up")
        # bluetoothd re-powers controllers on adoption; wait until it has taken
        # hci0, otherwise blooter may race it for the default adapter.
        bt.wait_for_output(r"hci0", 15.0, "bluetoothd to adopt hci0")

    def _connections(self, index):
        out = btmgmt(index, "con")
        return re.findall(r"([0-9A-Fa-f:]{17})", out)

    def reset_link_state(self):
        """Drop any link left up by the previous test.

        Tests share one bluetoothd and one pair of controllers, so a link left
        established carries into the next test. The bond is deliberately *kept*
        (see `pair_controllers`) -- only the connection is torn down.
        """
        for index, peer in ((0, self.addresses[1]), (1, self.addresses[0])):
            btmgmt(index, "disconnect", peer)

        # Disconnect is asynchronous; the next test must not dial into a link
        # that is still tearing down.
        try:
            wait_for(lambda: not self._connections(0) and not self._connections(1),
                     10.0, "the previous link to drop")
        except HarnessError:
            log("WARNING: link did not drop cleanly; continuing")

    def stop(self):
        for proc in reversed(self.procs):
            proc.stop()
        self.procs = []

    def dump_logs(self):
        for proc in self.procs:
            out = proc.output().strip()
            if out:
                print(f"\n--- {proc.name} ({proc.log_path}) ---", flush=True)
                print("\n".join(out.splitlines()[-40:]), flush=True)


class Blooter:
    """The binary under test, in FIFO input mode.

    FIFO mode (`-f`) is what makes this testable without evdev: it sidesteps
    /dev/input and udev entirely, so the test writes `struct input_event`
    records straight into blooter's input pipeline and asserts on the HID
    reports that come out the other end.
    """

    def __init__(self, binary, extra_args=(), fifo=None, protocol="classic",
                 batch="none"):
        self.binary = binary
        self.fifo_path = fifo or os.path.join(RUNDIR, "blooter.fifo")
        self.extra_args = list(extra_args)
        self.protocol = protocol
        # Default to unbatched so a report arrives per injected frame; tests
        # that exercise batching pass batch="auto" or a millisecond count.
        self.batch = batch
        self.config_path = os.path.join(RUNDIR, "blooter.toml")
        self.proc = None
        self._fifo_fd = None

    def start(self):
        if os.path.exists(self.fifo_path):
            os.unlink(self.fifo_path)
        os.mkfifo(self.fifo_path, 0o600)
        # The transport is always pinned rather than left to the default, so a
        # change of default cannot silently move a test to the other transport.
        with open(self.config_path, "w") as fh:
            fh.write(f'[connection]\nprotocol = "{self.protocol}"\n')
            # Pointer batching is pinned per test rather than left to the
            # default, so report-content assertions do not depend on timing.
            # A millisecond count is a TOML integer, the modes are strings.
            batch = (self.batch if isinstance(self.batch, int)
                     else f'"{self.batch}"')
            fh.write(f'[pointer]\nbatch = {batch}\n')

        env = dict(os.environ, RUST_BACKTRACE="1")
        # Full adapter setup is deliberately left on (no `-n`): blooter making
        # itself discoverable and connectable is part of what is under test, and
        # without it the host's dial page-times-out. The menu stays out of the
        # way regardless because stdin is /dev/null, so blooter infers
        # non-interactive. `-d` gives per-report debug logging for failures.
        argv = [self.binary, "-f", self.fifo_path, "-c", self.config_path,
                "-d"] + self.extra_args
        self.proc = Process("blooter", argv, env=env)
        self.proc.wait_for_output(
            r"ready to accept connections", 20.0, "blooter to be ready")

        # Open the write end only once blooter is up. blooter's reader blocks in
        # open(2) until a writer appears and reopens on EOF, so this fd is held
        # for the process lifetime to keep one continuous stream.
        self._fifo_fd = os.open(self.fifo_path, os.O_WRONLY)
        return self

    def send_events(self, *events):
        if self._fifo_fd is None:
            raise HarnessError("blooter not started")
        os.write(self._fifo_fd, b"".join(events))

    def key(self, code, pressed):
        self.send_events(input_event(EV_KEY, code, 1 if pressed else 0),
                         input_event(EV_SYN, SYN_REPORT, 0))

    def rel(self, code, value):
        self.send_events(input_event(EV_REL, code, value),
                         input_event(EV_SYN, SYN_REPORT, 0))

    def rel_frame(self, *axes):
        """One input frame carrying several relative axes, e.g.
        `rel_frame((REL_X, 3), (REL_Y, -4))`. blooter merges the whole frame
        into a single report."""
        self.send_events(*[input_event(EV_REL, c, v) for c, v in axes],
                         input_event(EV_SYN, SYN_REPORT, 0))

    def output(self):
        return self.proc.output() if self.proc else ""

    def wait_for_output(self, pattern, timeout, what=None, after=0):
        return self.proc.wait_for_output(pattern, timeout, what, after=after)

    def stop(self):
        if self._fifo_fd is not None:
            try:
                os.close(self._fifo_fd)
            except OSError:
                pass
            self._fifo_fd = None
        rc = self.proc.stop() if self.proc else None
        if os.path.exists(self.fifo_path):
            try:
                os.unlink(self.fifo_path)
            except OSError:
                pass
        return rc


class FakeHost:
    """A host that connects to blooter over real L2CAP, as a machine would.

    Dials the control PSM then the interrupt PSM (blooter requires the second
    within 3s of the first), then receives HID input reports on the interrupt
    channel.
    """

    def __init__(self, local_addr, peer_addr):
        self.local_addr = local_addr
        self.peer_addr = peer_addr
        self.ctrl = None
        self.intr = None

    def _socket(self):
        sock = socket.socket(AF_BLUETOOTH, socket.SOCK_SEQPACKET, BTPROTO_L2CAP)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        # Ask for "low" security explicitly. Without this the kernel escalates
        # to an authenticated link, both bluetoothd-managed controllers try to
        # bond at once, the collision fails authentication, and every later
        # connect is refused with EACCES. blooter's own listeners take the same
        # default, and a real HID host bonds via the agent path, not here.
        sock.setsockopt(SOL_BLUETOOTH, BT_SECURITY,
                        struct.pack("BB", BT_SECURITY_LOW, 0))
        # Binding to our controller's address selects which adapter dials.
        sock.bind((self.local_addr, 0))
        return sock

    def connect(self, timeout=15.0):
        self.ctrl = self._socket()
        self.ctrl.settimeout(timeout)
        self.ctrl.connect((self.peer_addr, CONTROL_PSM))
        # Deliberately immediate: blooter's await_interrupt allows 3s, and a
        # slow second dial is a real failure mode we do not want to mask.
        self.intr = self._socket()
        self.intr.settimeout(timeout)
        self.intr.connect((self.peer_addr, INTERRUPT_PSM))
        return self

    def connect_control_only(self, timeout=15.0):
        """Dial the control PSM and stop there, leaving blooter waiting for an
        interrupt channel that never arrives."""
        self.ctrl = self._socket()
        self.ctrl.settimeout(timeout)
        self.ctrl.connect((self.peer_addr, CONTROL_PSM))
        return self

    def recv_report(self, timeout=5.0):
        """Next HID input report from the interrupt channel."""
        self.intr.settimeout(timeout)
        try:
            data = self.intr.recv(64)
        except socket.timeout:
            raise AssertionError(
                f"no HID report within {timeout}s") from None
        if not data:
            raise AssertionError("interrupt channel closed by blooter")
        return data

    def drain(self, settle=0.4):
        """Collect any reports already queued (e.g. state emitted on connect),
        so a test can assert on what follows from its own input."""
        collected = []
        self.intr.settimeout(settle)
        while True:
            try:
                data = self.intr.recv(64)
            except socket.timeout:
                return collected
            if not data:
                return collected
            collected.append(data)

    def send_ctrl(self, payload):
        self.ctrl.send(payload)

    def unplug(self):
        """Virtual-cable unplug: tells blooter this host is forgetting it."""
        self.ctrl.send(bytes([VIRTUAL_CABLE_UNPLUG]))

    def close(self):
        for sock in (self.intr, self.ctrl):
            if sock is not None:
                try:
                    sock.close()
                except OSError:
                    pass
        self.intr = self.ctrl = None


class LeHost:
    """A host that connects to blooter over real LE, as a machine would.

    The LE counterpart of `FakeHost`. Python's socket module cannot express an
    ATT connection (no CID or address-type in its L2CAP sockaddr), so this
    drives bluez's own `btgatt-client`, which opens its own ATT socket on hci1 —
    just like FakeHost's raw L2CAP sockets, and for the same reason: no second
    bluetoothd, so the shared-agent artifact (TODO.md) stays out of it.

    Subscribing needs no bond: blooter's CCCDs are not encryption-gated (only
    the Report *reads* and the Report Map are), and the CCCD subscribe is what
    blooter treats as "connected" (design/ARCH.md §4.2).
    """

    ANSI = re.compile(r"\x1b\[[0-9;]*m")
    HID_SERVICE_UUID = "00001812"
    REPORT_UUID = "00002a4d"
    DISCOVERY_DONE = r"GATT discovery procedures complete"
    NOTIFICATION = r"Handle Value Not/Ind: (0x[0-9a-f]+) - \(\d+ bytes\): ([0-9a-f ]+)"

    def __init__(self, binary, adapter, peer_addr):
        self.binary = binary
        self.adapter = adapter
        self.peer_addr = peer_addr
        self.proc = None

    def output(self):
        return self.ANSI.sub("", self.proc.output()) if self.proc else ""

    def connect(self, timeout=20.0):
        """Open the LE link and wait for GATT discovery to finish."""
        self.proc = Process(
            "btgatt-client",
            [self.binary, "-i", self.adapter, "-d", self.peer_addr,
             "-t", "public"],
            stdin_pipe=True)
        wait_for(lambda: re.search(self.DISCOVERY_DONE, self.output()),
                 timeout, "btgatt-client to finish GATT discovery")
        return self

    def report_handles(self):
        """Value handles of the HID service's Report characteristics.

        Parsed rather than hardcoded: bluetoothd assigns handles at
        registration time and neither the values nor the order of the two
        characteristics is stable between runs.
        """
        handles, in_hid = [], False
        for line in self.output().splitlines():
            line = line.strip()
            if line.startswith("service -"):
                in_hid = self.HID_SERVICE_UUID in line
            elif line.startswith("charac -") and in_hid \
                    and self.REPORT_UUID in line:
                handle = int(re.search(r"value: (0x[0-9a-f]+)", line).group(1), 16)
                if handle not in handles:
                    handles.append(handle)
        return handles

    LAYOUT_UUID_PREFIX = "626c6f74-6572-4c41-594f-5554"

    def layout_uuid(self):
        """UUID of the vendor layout characteristic, whose last four bytes carry
        the HID descriptor fingerprint (design/CONNECTION.md §7.2b), or None."""
        for line in self.output().splitlines():
            line = line.strip()
            if line.startswith("charac -") and self.LAYOUT_UUID_PREFIX in line:
                return re.search(r"uuid: (\S+)", line).group(1)
        return None

    def subscribe(self, timeout=10.0):
        """Write every Report CCCD, which is what makes blooter "connected"."""
        handles = self.report_handles()
        if not handles:
            raise AssertionError(
                f"no Report characteristics in blooter's GATT tree:\n{self.output()}")
        for handle in handles:
            seen = len(re.findall(r"Registered notify handler", self.output()))
            self.proc.write_stdin(f"register-notify {handle:#06x}\n")
            self.proc.wait_for_output(
                r"Registered notify handler", timeout,
                f"the CCCD subscribe on {handle:#06x}", after=seen)
        return self

    def notifications(self):
        """Every notification received so far, as (handle, payload) pairs."""
        return [(int(h, 16), bytes.fromhex(payload.replace(" ", "")))
                for h, payload in re.findall(self.NOTIFICATION, self.output())]

    def wait_for_notification(self, payload, timeout=5.0):
        """Wait for `payload` to arrive on any Report characteristic.

        Matched on the payload rather than a specific handle: which of the two
        Report characteristics is the mouse and which the keyboard depends on
        the handle order bluetoothd happened to assign, and the point of the
        assertion is that the report reached the host at all.
        """
        def check():
            return any(got == payload for _handle, got in self.notifications())

        try:
            wait_for(check, timeout, f"a notification of {describe(payload)}")
        except HarnessError:
            got = [describe(p) for _h, p in self.notifications()]
            raise AssertionError(
                f"no notification of {describe(payload)} within {timeout}s"
                f"\n  received: {got or '<none>'}") from None

    def close(self):
        if self.proc is not None:
            self.proc.stop()
            self.proc = None


# --------------------------------------------------------------------------
# Expected reports
# --------------------------------------------------------------------------

def le_payload(report):
    """The notification value for a wire report: the LE transport strips the
    0xA1 HIDP header and the report id, which route to a characteristic
    instead (design/ARCH.md §4.2)."""
    return report[2:]


def keyboard_report(modifiers=0, keys=()):
    """An 11-byte keyboard report, as InputState::keyboard_report builds it."""
    pressed = list(keys) + [0] * (8 - len(keys))
    return bytes([HIDP_DATA_INPUT, REPORT_ID_KEYBOARD, modifiers] + pressed)


def mouse_report(buttons=0, x=0, y=0, wheel=0):
    """A 6-byte mouse report, as InputState::mouse_report builds it."""
    return bytes([HIDP_DATA_INPUT, REPORT_ID_MOUSE, buttons,
                  x & 0xFF, y & 0xFF, wheel & 0xFF])


def describe(report):
    return " ".join(f"{b:02x}" for b in report) if report else "<empty>"


def assert_report(got, expected, what):
    """Compare two reports, showing both as hex on failure -- a bare
    `assert a == b` on bytes says nothing useful about which byte differs."""
    if got != expected:
        diff = ""
        if len(got) == len(expected):
            positions = [i for i, (a, b) in enumerate(zip(got, expected)) if a != b]
            diff = f"\n  differs at byte(s): {positions}"
        raise AssertionError(
            f"{what}\n  expected: {describe(expected)}"
            f"\n  got:      {describe(got)}{diff}")


# --------------------------------------------------------------------------
# Minimal test runner (the guest has no pytest)
# --------------------------------------------------------------------------

class Registry:
    def __init__(self):
        self.tests = []

    def test(self, func):
        self.tests.append(func)
        return func

    def run(self, stack, binary, only=None):
        passed, failed, skipped = [], [], []
        for func in self.tests:
            name = func.__name__
            if only and only not in name:
                skipped.append(name)
                continue
            print(f"\n>>> {name}", flush=True)
            stack.reset_link_state()
            ctx = TestContext(stack, binary)
            start = time.monotonic()
            try:
                func(ctx)
            except Exception as exc:  # noqa: BLE001 - reported, not swallowed
                elapsed = time.monotonic() - start
                failed.append((name, exc))
                print(f"<<< FAIL {name} ({elapsed:.1f}s): "
                      f"{type(exc).__name__}: {exc}", flush=True)
                traceback.print_exc()
                ctx.dump_diagnostics()
            else:
                elapsed = time.monotonic() - start
                passed.append(name)
                print(f"<<< pass {name} ({elapsed:.1f}s)", flush=True)
            finally:
                ctx.cleanup()

        print("\n" + "=" * 60, flush=True)
        print(f"passed: {len(passed)}   failed: {len(failed)}"
              + (f"   skipped: {len(skipped)}" if skipped else ""), flush=True)
        for name, exc in failed:
            print(f"  FAILED {name}: {type(exc).__name__}: {exc}", flush=True)
        return 1 if failed else 0


class TestContext:
    """Per-test state: a fresh blooter and any hosts it connected to."""

    def __init__(self, stack, binary):
        self.stack = stack
        self.binary = binary
        self.blooter = None
        self.hosts = []

    def start_blooter(self, extra_args=(), protocol="classic", batch="none"):
        self.blooter = Blooter(self.binary, extra_args=extra_args,
                               protocol=protocol, batch=batch).start()
        return self.blooter

    def host(self):
        """A fake host on hci1, dialing blooter on hci0."""
        h = FakeHost(self.stack.addresses[1], self.stack.addresses[0])
        self.hosts.append(h)
        return h

    def le_host(self):
        """An LE host on hci1, connecting to blooter on hci0."""
        h = LeHost(os.environ["BTGATT_CLIENT"], "hci1", self.stack.addresses[0])
        self.hosts.append(h)
        return h

    CONNECTED_PATTERN = r"[Hh]ost connected"

    def connected_host(self):
        """The common case: a host that has completed both PSM connections and
        which blooter has logged as connected.

        Counts the connection lines already logged first, so a reconnect waits
        for its own session rather than being satisfied by the previous one.
        """
        seen = self.blooter.proc.match_count(self.CONNECTED_PATTERN)
        host = self.host().connect()
        self.blooter.wait_for_output(
            self.CONNECTED_PATTERN, 10.0,
            "blooter to report the host connected", after=seen)
        return host

    def connected_le_host(self):
        """The LE equivalent: a host that has opened the link and subscribed to
        the Report CCCDs, which is what blooter counts as connected."""
        seen = self.blooter.proc.match_count(self.CONNECTED_PATTERN)
        host = self.le_host().connect().subscribe()
        self.blooter.wait_for_output(
            self.CONNECTED_PATTERN, 10.0,
            "blooter to report the LE host connected", after=seen)
        return host

    def dump_diagnostics(self):
        if self.blooter:
            out = self.blooter.output().strip()
            if out:
                print("\n--- blooter output (tail) ---", flush=True)
                print("\n".join(out.splitlines()[-40:]), flush=True)
        self.stack.dump_logs()

    def cleanup(self):
        for host in self.hosts:
            host.close()
        self.hosts = []
        if self.blooter:
            self.blooter.stop()
            self.blooter = None
