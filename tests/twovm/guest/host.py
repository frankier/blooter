"""The `host` guest: an ordinary Linux Bluetooth host.

It is never taught anything about blooter. It runs stock `bluetoothd` *with* its
input plugin — the one thing `tests/btvirt` must disable — an auto-accepting
pairing agent, and nothing else. Pairing is initiated from here, and the HID
device that results is driven by this kernel's own HID stack.

That asymmetry is the point (design/TESTS.md §3): if a test passes, it passes
against a stock BlueZ host doing ordinary things, so the assertion at the end is
not "the right bytes went out" but "a stock Linux host ended up with a working
keyboard and mouse".

Everything goes through **one** long-lived `bluetoothctl` on a pty. One, because
two would each register an agent and the second would displace the first; on a
pty, because bluetoothctl block-buffers down a pipe and a scripted conversation
with it then goes nowhere.
"""

import glob
import os
import re
import selectors
import time

from base import RoleAgent
from common import (
    HarnessError,
    PtyProcess,
    log,
    parse_input_events,
    wait_for,
)
from common.evdev import INPUT_EVENT

BLUETOOTHCTL = os.environ.get("BLUETOOTHCTL", "/usr/bin/bluetoothctl")


class Bluetoothctl:
    """A scripted conversation with `bluetoothctl`."""

    def __init__(self):
        self.proc = PtyProcess("bluetoothctl", [BLUETOOTHCTL])
        self.wait(r"Agent registered|\[bluetooth\]|#", 20.0,
                  "bluetoothctl to start")
        # Its own agent comes up before the adapter does, so "started" is not
        # "usable": the first adapter command otherwise answers "No default
        # controller available" and the whole run stops for a race. Asked for
        # rather than waited for, because whether bluetoothctl volunteers a
        # `[NEW] Controller` line depends on when the object manager delivered
        # it -- but `list` always answers.
        def listed():
            self.send("list")
            time.sleep(0.5)
            return re.search(r"Controller ([0-9A-F]{2}:){5}[0-9A-F]{2}",
                             self.proc.output())

        wait_for(listed, 30.0, "bluetoothctl to see the adapter", interval=1.0)

    def send(self, line):
        self.proc.write_stdin(line + "\n")

    def wait(self, pattern, timeout, what, after=0):
        return self.proc.wait_for_output(pattern, timeout, what, after=after)

    def run(self, line, expect=None, fail=None, timeout=30.0):
        """Send a command and wait for one of two outcomes.

        The count of prior matches is taken *before* sending, so a second `pair`
        is not satisfied by the first one's "Pairing successful" — the same
        trap `Process.wait_for_output`'s `after` exists for, and the one that
        makes a re-pair test pass without re-pairing anything.
        """
        if expect is None:
            self.send(line)
            return self.proc.output()

        seen_ok = self.proc.match_count(expect)
        seen_bad = self.proc.match_count(fail) if fail else 0
        self.send(line)

        combined = f"(?:{expect})" + (f"|(?:{fail})" if fail else "")

        def done():
            if self.proc.match_count(expect) > seen_ok:
                return "ok"
            if fail and self.proc.match_count(fail) > seen_bad:
                return "failed"
            return None

        try:
            outcome = wait_for(done, timeout, f"{line!r} to report {combined}")
        except HarnessError:
            # A bluetoothctl command that neither succeeded nor failed said
            # *something*, and that something is the whole diagnosis.
            raise HarnessError(
                f"{line!r} reported neither {combined} within {timeout}s:\n"
                + "\n".join(self.proc.output().splitlines()[-25:])) from None
        if outcome == "failed":
            raise HarnessError(
                f"{line!r} failed:\n"
                + "\n".join(self.proc.output().splitlines()[-25:]))
        return self.proc.output()

    def output(self):
        return self.proc.output()

    def stop(self):
        self.proc.stop()


class InputReader:
    """evdev nodes on the host, read as any application would.

    The names are matched against `/sys/class/input/event*/device/name` rather
    than guessed by number (design/TESTS.md §4): which `eventN` the kernel hands
    out depends on everything else that has been created and destroyed in the
    run, and the numbers move between tests.
    """

    def __init__(self):
        self.fds = {}
        self.sel = selectors.DefaultSelector()

    def open(self, names):
        self.close()
        for name in names:
            node = find_event_node(name)
            if node is None:
                raise HarnessError(f"no input device named {name!r} on the host")
            fd = os.open(node, os.O_RDONLY | os.O_NONBLOCK)
            self.fds[name] = fd
            self.sel.register(fd, selectors.EVENT_READ, name)
            log(f"reading {name!r} from {node}")
        return {name: find_event_node(name) for name in names}

    def read(self, timeout=3.0, settle=0.15):
        """Every event that arrives within `timeout`, as (name, type, code,
        value). Returns as soon as a frame has arrived and gone quiet."""
        out, deadline = [], time.monotonic() + timeout
        while time.monotonic() < deadline:
            for key, _mask in self.sel.select(timeout=0.05):
                try:
                    data = os.read(key.fd, INPUT_EVENT.size * 64)
                except (BlockingIOError, OSError):
                    continue
                out += [(key.data, *event) for event in parse_input_events(data)]
            if out:
                deadline = min(deadline, time.monotonic() + settle)
        return out

    def close(self):
        for fd in self.fds.values():
            self.sel.unregister(fd)
            os.close(fd)
        self.fds = {}


def find_event_node(name):
    for path in sorted(glob.glob("/sys/class/input/event*")):
        try:
            with open(os.path.join(path, "device", "name")) as fh:
                if fh.read().strip() == name:
                    return "/dev/input/" + os.path.basename(path)
        except OSError:
            continue
    return None


def input_device_names():
    names = []
    for path in sorted(glob.glob("/sys/class/input/event*")):
        try:
            with open(os.path.join(path, "device", "name")) as fh:
                names.append(fh.read().strip())
        except OSError:
            continue
    return names


class HostAgent(RoleAgent):
    role = "host"
    # Nothing disabled. bluetoothd's `input` plugin is what turns a paired HID
    # device into a uhid node and hence into evdev events, and that whole path
    # is what this suite exists to exercise.
    disable_plugins = ()
    # ...and `uhid` is the node it needs. Without it, HOGP fails with a single
    # "Unable to create UHID" line and the host silently never gets a keyboard.
    modules = ("uhid",)

    def __init__(self):
        super().__init__()
        self.ctl = None
        self.reader = InputReader()
        self.commands.update({
            "start_agent": self.start_agent,
            "discover": self.discover,
            "await_device": self.await_device,
            "pair": self.pair,
            "trust": self.trust,
            "connect": self.connect,
            "disconnect": self.disconnect,
            "remove": self.remove,
            "info": self.info,
            "paired": self.paired,
            "ctl": self.ctl_command,
            "ctl_output": self.ctl_output,
            "await_input_devices": self.await_input_devices,
            "input_devices": self.input_devices,
            "open_inputs": self.open_inputs,
            "read_events": self.read_events,
            "close_inputs": self.close_inputs,
            "restart_stack": self.restart_stack,
        })

    # -- the agent ----------------------------------------------------------

    def start_agent(self):
        """An auto-accepting pairing agent, as a headless host would have.

        `NoInputNoOutput` negotiates Just Works, which needs no confirmation on
        this side — so the only agent decision left in the whole flow is
        blooter's own, which is exactly the code path `tests/btvirt` cannot
        reach and where the `AuthenticationFailed` bug lived.
        """
        if self.ctl is not None:
            self.ctl.stop()
        self.ctl = Bluetoothctl()
        # bluetoothctl registers a KeyboardDisplay agent of its own on startup,
        # so ours has to displace it: `agent NoInputNoOutput` on top of that one
        # answers "Agent is already registered" and quietly leaves the wrong
        # capability in place, which would put an interactive confirmation in
        # the middle of every pairing here.
        self.ctl.run("agent off", expect=r"Agent unregistered|No agent is registered",
                     timeout=15.0)
        self.ctl.run("agent NoInputNoOutput", expect=r"Agent registered",
                     fail=r"Failed to register agent|already registered",
                     timeout=15.0)
        self.ctl.run("default-agent", expect=r"Default agent request successful",
                     fail=r"Failed to set|No agent is registered", timeout=15.0)
        # A host the Classic menu can list has to be findable at all: blooter's
        # host picker builds its list from a BR/EDR discovery scan (§6).
        self.ctl.run("discoverable on", expect=r"Changing .*succeeded",
                     fail=r"Failed to set", timeout=15.0)
        return True

    def _ctl(self):
        if self.ctl is None:
            raise HarnessError("start_agent has not been called")
        return self.ctl

    # -- discovery and bonding ----------------------------------------------

    def discover(self, on=True):
        self._ctl().send(f"scan {'on' if on else 'off'}")
        return True

    def await_device(self, address, timeout=45.0):
        """Wait until this host has a device object for `address`.

        A scan is left running for the duration: on BLE the object only exists
        once an advertisement has been received, and on Classic once an inquiry
        response has.
        """
        ctl = self._ctl()
        # Only what arrives from *this* scan counts. One bluetoothctl serves a
        # whole test, so its transcript still contains every mention of the
        # address from before the device was removed -- and a plain "is it in
        # the output?" is satisfied by that immediately, after which `pair`
        # blocks on a device the host does not actually know about. Which is a
        # long way from what the failure then looks like.
        mark = len(ctl.output())
        ctl.send("scan on")
        try:
            wait_for(lambda: address.upper() in ctl.output()[mark:].upper(),
                     timeout, f"the host to discover {address}")
        finally:
            ctl.send("scan off")
        return True

    def pair(self, address, timeout=60.0, trust=True):
        """Pair with blooter, from here, over whichever transport it offers.

        No `-t`: which transport the bond takes is decided by what blooter is
        actually advertising, not by the test, so a blooter configured for BLE
        that somehow still offered BR/EDR would be caught rather than papered
        over.
        """
        ctl = self._ctl()
        self.await_device(address, timeout=timeout)
        ctl.run(f"pair {address}", expect=r"Pairing successful",
                fail=r"Failed to pair", timeout=timeout)
        if trust:
            self.trust(address, True)
        return self.info(address)

    def trust(self, address, on=True):
        # bluetoothctl routes trust through generic_callback, which prints
        # "Changing <value> succeeded", where <value> is the whole argument
        # ("trust on") -- there is no message naming the verb on its own.
        verb = "trust" if on else "untrust"
        self._ctl().run(f"{verb} {address}",
                        expect=r"Changing .*succeeded", fail=r"Failed to set",
                        timeout=20.0)
        return self.info(address)

    def connect(self, address, timeout=45.0):
        self._ctl().run(f"connect {address}", expect=r"Connection successful",
                        fail=r"Failed to connect", timeout=timeout)
        return self.info(address)

    def disconnect(self, address, timeout=30.0):
        self._ctl().run(f"disconnect {address}",
                        expect=r"Disconnection successful",
                        fail=r"Failed to disconnect", timeout=timeout)
        return True

    def remove(self, address, timeout=30.0):
        """Delete the bond from this side -- the out-of-band damage of §6.

        Done here with `bluetoothctl`, never through blooter's menu, because the
        whole question a divergence test asks is what happens when the *other*
        machine changes its mind without telling blooter.
        """
        self._ctl().run(f"remove {address}", expect=r"Device has been removed",
                        fail=r"Failed to remove device|not available",
                        timeout=timeout)
        return True

    def info(self, address):
        ctl = self._ctl()
        before = len(ctl.output())
        ctl.send(f"info {address}")
        time.sleep(0.4)
        return ctl.output()[before:]

    def paired(self, address):
        return bool(re.search(r"Paired:\s*yes", self.info(address)))

    def ctl_command(self, line, expect=None, fail=None, timeout=30.0):
        return self._ctl().run(line, expect=expect, fail=fail, timeout=timeout)

    def ctl_output(self, tail=60):
        if self.ctl is None:
            return ""
        return "\n".join(self.ctl.output().splitlines()[-tail:])

    # -- the HID devices the kernel builds ----------------------------------

    def await_input_devices(self, names, timeout=45.0):
        """Wait for the kernel's HID parser to have built these input devices.

        This is the intermediate observable that the report descriptor is really
        being tested against (design/TESTS.md §4): the host reading the Report
        Map, parsing it, and creating one input device per application
        collection. A descriptor that no real host accepts fails here, loudly,
        before a single report is sent.
        """
        names = list(names)

        def ready():
            present = input_device_names()
            return all(n in present for n in names)

        try:
            wait_for(ready, timeout, f"the host to create {names}")
        except HarnessError:
            raise HarnessError(
                f"the host never created {names}; it has: {input_device_names()}"
            ) from None
        return {n: find_event_node(n) for n in names}

    def input_devices(self):
        return input_device_names()

    def open_inputs(self, names):
        return self.reader.open(names)

    def read_events(self, timeout=3.0):
        return self.reader.read(timeout=timeout)

    def close_inputs(self):
        self.reader.close()

    # -- teardown -----------------------------------------------------------

    def restart_stack(self, timeout=60.0):
        """Everything a host reboot would take with it, short of the kernel.

        The bluetoothctl conversation goes too -- its agent lives on the bus it
        is losing -- and comes back afterwards, so the host is left in the same
        state it boots into.
        """
        self.reader.close()
        if self.ctl is not None:
            self.ctl.stop()
            self.ctl = None
        self.stack.restart_bluetoothd()
        self.start_agent()
        return self.stack.address

    def shutdown(self):
        self.reader.close()
        if self.ctl is not None:
            self.ctl.stop()
            self.ctl = None
        super().shutdown()
