#!/usr/bin/env python3
"""Host side of the two-host suite: the radio, the two VMs, and the tests.

Slirp gives each guest its own isolated network, so the VMs can reach the host
but never each other (design/TESTS.md §2.1). Every piece of cross-VM
coordination therefore goes through here, which is less of a workaround than it
sounds: with the control plane routed through one process, a test that spans two
machines still reads as one linear story.

    hub  --TCP-->  dev agent   (blooter under test)
         --TCP-->  host agent  (stock BlueZ + kernel HID)

The only thing joining the two guests is the emulated radio: both run
`btproxy -c` against the one `btvirt -t` started here, and btvirt keeps every
client in a single `btdev_list` that inquiry and LE scan walk. Connection order
is each VM's identity (§2.3), so `start_radio` is issued to `dev` first and only
then to `host`.
"""

import json
import os
import socket
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))   # tests/, for `common`
sys.path.insert(0, HERE)                    # tests/twovm/, for `tests`

from common import HarnessError, Process, log, set_rundir, wait_for  # noqa: E402

BTVIRT = os.environ.get("BTVIRT", "")
BTVIRT_PORT = os.environ.get("BTVIRT_PORT", "45550")
HUB_PORT = int(os.environ.get("HUB_PORT", "45551"))
# btvirt -t binds 127.0.0.1, not 0.0.0.0, which is exactly why the guests need
# slirp's 10.0.2.2 alias for the host's loopback rather than a routable address.
HUB_BIND = "127.0.0.1"
GUEST_VIEW_OF_HOST = "10.0.2.2"


# --------------------------------------------------------------------------
# The wire
# --------------------------------------------------------------------------

class AgentConn:
    """One guest's connection. Requests are strictly sequential per guest."""

    def __init__(self, role, sock):
        self.role = role
        self.sock = sock
        self.stream = sock.makefile("rwb")
        self.lock = threading.Lock()
        self.seq = 0

    def call(self, cmd, _timeout=60.0, **args):
        """Send one command and block for its reply.

        The RPC deadline is `_timeout`, underscored on purpose: several guest
        commands take a `timeout` of their own (how long *they* should wait for
        a bond, an input device, an event), and the two must not collide. The
        RPC deadline is always the longer of the pair.
        """
        with self.lock:
            self.seq += 1
            ident = self.seq
            self.sock.settimeout(_timeout)
            self.stream.write(
                json.dumps({"id": ident, "cmd": cmd, "args": args}).encode()
                + b"\n")
            self.stream.flush()
            try:
                line = self.stream.readline()
            except socket.timeout:
                raise HarnessError(
                    f"{self.role}: {cmd} did not answer within {_timeout}s"
                ) from None
        if not line:
            raise HarnessError(f"{self.role}: agent disconnected during {cmd}")
        reply = json.loads(line)
        if not reply.get("ok"):
            trace = reply.get("trace", "")
            raise HarnessError(
                f"{self.role}.{cmd}: {reply.get('error')}"
                + (f"\n--- {self.role} traceback ---\n{trace}" if trace else ""))
        return reply.get("result")

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class Hub:
    """Accepts guest agents and hands them out by role."""

    def __init__(self, port=HUB_PORT):
        self.agents = {}
        self.ready = threading.Event()
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind((HUB_BIND, port))
        self.listener.listen(4)
        self.port = self.listener.getsockname()[1]
        self._thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._thread.start()
        log(f"hub listening on {HUB_BIND}:{self.port}")

    def _accept_loop(self):
        while True:
            try:
                sock, _peer = self.listener.accept()
            except OSError:
                return
            try:
                hello = json.loads(sock.makefile("rb").readline())
            except (OSError, ValueError):
                sock.close()
                continue
            role = hello.get("hello")
            log(f"{role} agent registered ({len(hello.get('commands', []))} commands)")
            self.agents[role] = AgentConn(role, sock)

    def await_agent(self, role, timeout=180.0):
        return wait_for(lambda: self.agents.get(role), timeout,
                        f"the {role} VM's agent to dial in")

    def close(self):
        for agent in self.agents.values():
            agent.close()
        self.listener.close()


# --------------------------------------------------------------------------
# The two guests, as objects a test can read
# --------------------------------------------------------------------------

class Vm:
    def __init__(self, agent):
        self.agent = agent
        self.address = None

    def call(self, cmd, **args):
        return self.agent.call(cmd, **args)

    def start_radio(self):
        result = self.call("start_radio", _timeout=180.0)
        self.address = result["address"]
        log(f"{self.agent.role}: hci0 is {self.address}  [{result['settings']}]")
        return self.address

    def bonds(self):
        """This side's bond store: `{address: {"linkkey":…, "ltk":…}}`."""
        return self.call("bonded")

    def bonded_to(self, address):
        return self.bonds().get(address.upper())

    def unbond_all(self):
        return self.call("unbond_all", _timeout=120.0)

    def ensure_radio(self):
        """Refresh this guest's address, rebuilding its bridge if it has gone.

        The address is re-read rather than remembered, because a rebuilt bridge
        is a new btvirt client and so a new BD_ADDR (see `respawn_radio`).
        """
        address = self.call("ensure_radio", _timeout=180.0)
        if address != self.address:
            log(f"{self.agent.role}: address is now {address} "
                f"(was {self.address}); its bridge had to be rebuilt")
            self.address = address
        return address

    def wipe_bonds(self):
        return self.call("wipe_bonds", _timeout=120.0)

    def restart_bluetoothd(self):
        return self.call("restart_bluetoothd", _timeout=120.0)

    def logs(self, tail=200):
        return self.call("logs", tail=tail)

    def btmon_start(self, tag):
        return self.call("btmon_start", tag=tag)

    def btmon_stop(self):
        return self.call("btmon_stop")

    def btmon_output(self, tail=120):
        return self.call("btmon_output", tail=tail)


class DevVm(Vm):
    """blooter's machine."""

    CONNECTED = r"[Hh]ost connected"
    DISCONNECTED = r"host disconnected"

    def start_blooter(self, protocol="classic", **kwargs):
        self.call("start_blooter", _timeout=180.0, protocol=protocol, **kwargs)
        return self

    def stop_blooter(self):
        return self.call("stop_blooter", _timeout=60.0)

    def log(self, tail=None):
        return self.call("blooter_output", tail=tail)

    def alive(self):
        return self.call("blooter_alive")

    def count(self, pattern):
        return self.call("blooter_match_count", pattern=pattern)

    def wait_log(self, pattern, timeout=30.0, after=0, what=None):
        return self.call("wait_blooter_output", _timeout=timeout + 30.0,
                         pattern=pattern, timeout=timeout, after=after,
                         what=what)

    def await_session(self, timeout=60.0, after=None):
        """Wait for blooter to report a *new* host session.

        Counting first is what makes this usable for a reconnect: a plain "has
        it appeared?" check is satisfied by the previous session's line.
        """
        seen = self.count(self.CONNECTED) if after is None else after
        return self.wait_log(self.CONNECTED, timeout=timeout, after=seen)

    def sessions(self):
        return self.count(self.CONNECTED)

    # -- input --------------------------------------------------------------

    def key(self, code, pressed):
        return self.call("key", code=code, pressed=pressed)

    def tap(self, code):
        self.key(code, True)
        self.key(code, False)

    def rel(self, code, value):
        return self.call("rel", code=code, value=value)

    # -- menu ---------------------------------------------------------------

    def menu(self, keys):
        """Press keys in blooter's menu (needs `interactive=True`).

        Every scenario here has exactly one host in the list, so row 0 is
        already selected and `[f]`/`[u]` act on it without any navigation.
        """
        return self.call("menu_key", keys=keys)

    # -- bonds and state ----------------------------------------------------

    def remove_bond(self, address):
        return self.call("remove_bond", _timeout=60.0, address=address)

    def state_hosts(self):
        return self.call("state_hosts")

    # -- uinput -------------------------------------------------------------

    def make_uinput(self, keyboard_keys, mouse_keys, mouse_rels):
        return self.call("make_uinput", _timeout=60.0,
                         keyboard_keys=list(keyboard_keys),
                         mouse_keys=list(mouse_keys),
                         mouse_rels=list(mouse_rels))

    def uinput_key(self, which, code, pressed):
        return self.call("uinput_key", which=which, code=code, pressed=pressed)

    def uinput_rel(self, which, code, value):
        return self.call("uinput_rel", which=which, code=code, value=value)

    def watch_input(self, which):
        return self.call("watch_input", which=which)

    def watch_read(self, timeout=1.0):
        return [tuple(e) for e in
                self.call("watch_read", _timeout=timeout + 30.0, timeout=timeout)]


class HostVm(Vm):
    """The stock BlueZ machine."""

    def start_agent(self):
        return self.call("start_agent", _timeout=120.0)

    def pair(self, address, trust=True):
        return self.call("pair", _timeout=240.0, address=address, trust=trust)

    def connect(self, address):
        return self.call("connect", _timeout=180.0, address=address)

    def reconnect(self, address):
        """Drop the link, then bring it back -- the host-side "reconnect".

        A bare `connect` is not one. bluetoothd answers it immediately with
        success whenever it still believes the device is connected, and it
        often does: blooter exiting closes its L2CAP channels without
        necessarily taking the ACL down, so the host is left connected to
        nothing and the HID profile is never re-established. Disconnecting
        first is both what makes the reconnect real and what a user does.
        """
        try:
            self.disconnect(address)
        except HarnessError:
            pass  # already down, which is the state we wanted
        return self.connect(address)

    def disconnect(self, address):
        return self.call("disconnect", _timeout=90.0, address=address)

    def remove(self, address):
        return self.call("remove", _timeout=90.0, address=address)

    def trust(self, address, on=True):
        return self.call("trust", _timeout=90.0, address=address, on=on)

    def info(self, address):
        return self.call("info", _timeout=60.0, address=address)

    def paired(self, address):
        return self.call("paired", _timeout=60.0, address=address)

    def ctl(self, line, expect=None, fail=None, timeout=60.0):
        return self.call("ctl", _timeout=timeout + 30.0, line=line,
                         expect=expect, fail=fail, timeout=timeout)

    def ctl_output(self, tail=60):
        return self.call("ctl_output", tail=tail)

    def restart_stack(self):
        return self.call("restart_stack", _timeout=240.0)

    # -- the HID devices the kernel builds ----------------------------------

    def await_input_devices(self, *names, timeout=60.0):
        return self.call("await_input_devices", _timeout=timeout + 30.0,
                         names=list(names), timeout=timeout)

    def input_devices(self):
        return self.call("input_devices")

    def open_inputs(self, *names):
        return self.call("open_inputs", names=list(names))

    def read_events(self, timeout=3.0):
        return [tuple(e) for e in self.call("read_events", _timeout=timeout + 30.0, timeout=timeout)]

    def close_inputs(self):
        return self.call("close_inputs")


# --------------------------------------------------------------------------
# The VMs themselves
# --------------------------------------------------------------------------

def boot_vm(role, hub_port, name=None):
    """Boot one guest and leave its agent to dial in.

    Same `vng -r --user root` invocation as `tests/btvirt`, plus
    `--network user`: slirp is what lets the guest reach the host's loopback at
    10.0.2.2, which is where both the radio and this hub live.
    """
    guest = os.path.join(HERE, "guest", "run-agent.sh")
    env = " ".join([
        f"ROLE={role}",
        f"HUB_ADDR={GUEST_VIEW_OF_HOST}",
        f"HUB_PORT={hub_port}",
        f"RADIO_ADDR={GUEST_VIEW_OF_HOST}",
        f"RADIO_PORT={BTVIRT_PORT}",
        f"BTPROXY='{os.environ['BTPROXY']}'",
        f"BTMGMT='{os.environ['BTMGMT']}'",
        f"BTMON='{os.environ.get('BTMON', '')}'",
        f"BLOOTER='{os.environ.get('BLOOTER', '')}'",
    ])
    argv = ["vng", "-r"]
    if os.environ.get("VNG_KERNEL"):
        argv.append(os.environ["VNG_KERNEL"])
    argv += ["--user", "root", "--network", "user",
             "--memory", os.environ.get("VNG_MEMORY", "1G"),
             "-e", f"{env} {guest}"]
    log(f"booting the {role} VM")
    return Process(name or f"vm-{role}", argv)


def require_free_port(port, what, timeout=90.0):
    """Wait for `port` to be bindable, and fail by name if it never is.

    Worth the trouble: btvirt prints "Listening TCP on …" *unconditionally*,
    even when its bind failed, so a leftover btvirt from an interrupted run is
    silently inherited instead of replaced -- and the symptom is a suite running
    against a radio with two stale controllers on it, failing somewhere else
    entirely.

    It waits rather than failing at once because btvirt binds without
    `SO_REUSEADDR`, so its port sits in `TIME_WAIT` for a minute after a clean
    run: back-to-back invocations are normal and must not need a stopwatch.
    """
    def free():
        probe = socket.socket()
        try:
            probe.bind((HUB_BIND, int(port)))
            return True
        except OSError:
            return False
        finally:
            probe.close()

    try:
        wait_for(free, timeout, f"{HUB_BIND}:{port} to become free for {what}",
                 interval=1.0)
    except HarnessError:
        raise HarnessError(
            f"{HUB_BIND}:{port} is still in use after {timeout}s, so {what} "
            "cannot start. A previous run is probably still up: "
            "pkill -f twovm/hub.py") from None


def start_radio_server():
    if not os.access(BTVIRT, os.X_OK):
        raise HarnessError(f"btvirt not executable: {BTVIRT}")
    require_free_port(BTVIRT_PORT, "the btvirt radio")
    # `-t` and nothing else. SERVER_TYPE_BREDRLE means one dual-mode server
    # serves both transports, and every accepted client becomes a btdev in the
    # shared list (design/TESTS.md §2). No `-l`: a local controller would need
    # /dev/vhci here, and the host side of this suite is entirely unprivileged
    # (§2.4) -- btvirt is the radio, not a controller.
    proc = Process("btvirt", [BTVIRT, "-t"])
    proc.wait_for_output(r"Listening TCP on", 15.0, "btvirt to open its port")
    return proc


# --------------------------------------------------------------------------
# Per-test state
# --------------------------------------------------------------------------

class TestContext:
    """What a test is handed: two machines and the bookkeeping between them."""

    def __init__(self, dev, host, name):
        self.dev = dev
        self.host = host
        self.name = name

    def start_blooter(self, protocol="classic", **kwargs):
        return self.dev.start_blooter(protocol=protocol, **kwargs)

    # -- the full round trip ------------------------------------------------

    KEYBOARD = "blooter Keyboard"
    MOUSE = "blooter Mouse"

    def pair_to_working_input(self, timeout=180.0):
        """From unpaired adapters to a keyboard and mouse on the host.

        The whole of J1, and the shape almost every other scenario ends with:
        the host initiates real SMP/SSP against blooter's real agent, the host
        kernel parses the Report Map it reads, and the input devices it builds
        are what "working" means here.
        """
        sessions = self.dev.sessions()
        self.host.pair(self.dev.address)
        # bluetoothctl drops the link once bonding is done, so the session that
        # carries HID is a separate, explicit connect -- which is also what a
        # desktop's Bluetooth panel does behind its "Connect" button.
        self.host.connect(self.dev.address)
        self.dev.await_session(timeout=timeout, after=sessions)
        return self.attach_input(timeout=timeout)

    def reconnect_host(self, timeout=90.0):
        """Drop the link, bring it back, and wait for the session that follows.

        The count of sessions blooter has already logged is taken *before* the
        reconnect, never after: a reconnect can complete before the next call is
        made, and an `await_session` that samples the count afterwards is then
        waiting for a *third* session that nobody is going to open. That failure
        looks exactly like "the host never reconnected", which is the one thing
        it is not.
        """
        sessions = self.dev.sessions()
        self.host.reconnect(self.dev.address)
        self.dev.await_session(timeout=timeout, after=sessions)
        return self.attach_input(timeout=timeout)

    def attach_input(self, timeout=90.0):
        """Wait for the host's HID devices and open them for reading.

        Needed again after every reconnect: the uhid device and its input nodes
        are destroyed when the link drops and rebuilt when it comes back, so a
        file descriptor from the previous session reads nothing forever.
        """
        nodes = self.host.await_input_devices(self.KEYBOARD, self.MOUSE,
                                              timeout=timeout)
        self.host.open_inputs(self.KEYBOARD, self.MOUSE)
        return nodes

    def expect_key(self, code, timeout=5.0):
        """Inject a key on dev; assert the host's evdev reports it."""
        self.dev.key(code, True)
        got = self.host.read_events(timeout=timeout)
        _assert_event(got, self.KEYBOARD, 0x01, code, 1, "key press")
        self.dev.key(code, False)
        got = self.host.read_events(timeout=timeout)
        _assert_event(got, self.KEYBOARD, 0x01, code, 0, "key release")

    def expect_motion(self, axis, value, timeout=5.0):
        self.dev.rel(axis, value)
        got = self.host.read_events(timeout=timeout)
        _assert_event(got, self.MOUSE, 0x02, axis, value, "pointer motion")

    def expect_working_input(self):
        """The assertion at the end of nearly every scenario."""
        from common import KEY_A, REL_X
        self.expect_key(KEY_A)
        self.expect_motion(REL_X, 5)

    # -- diagnostics --------------------------------------------------------

    def dump_diagnostics(self):
        """Two VMs make a failure twice as hard to read (design/TESTS.md §7.5),
        so a failure dumps everything from both at once rather than leaving it
        to be gathered by hand afterwards."""
        for vm, label in ((self.dev, "dev"), (self.host, "host")):
            try:
                print(f"\n===== {label} VM =====", flush=True)
                if label == "dev":
                    print("--- blooter ---", flush=True)
                    print(vm.log(tail=120), flush=True)
                else:
                    print("--- bluetoothctl ---", flush=True)
                    print(vm.ctl_output(tail=120), flush=True)
                print(vm.logs(tail=400), flush=True)
                print(f"--- {label} bonds: {vm.bonds()}", flush=True)
                btmon = vm.btmon_output(tail=80)
                if btmon:
                    print(f"--- {label} btmon ---\n{btmon}", flush=True)
            except HarnessError as exc:
                print(f"    | could not collect {label} diagnostics: {exc}",
                      flush=True)

    def cleanup(self):
        for action in (self.dev.stop_blooter,
                       self.host.close_inputs,
                       self.dev.btmon_stop,
                       self.host.btmon_stop):
            try:
                action()
            except HarnessError as exc:
                print(f"    | cleanup: {exc}", flush=True)


def _assert_event(events, device, type_, code, value, what):
    want = (device, type_, code, value)
    if want not in events:
        raise AssertionError(
            f"{what}: the host never reported {want}\n"
            f"  it reported: {events or '<nothing>'}")


def reset(dev, host):
    """Put both machines back to genuinely unpaired.

    Every scenario in §5 and §6 starts from unbonded adapters -- there is no
    `btmgmt pair` preamble anywhere in this suite, because the bond is the thing
    under test.
    """
    dev.stop_blooter()
    try:
        host.close_inputs()
    except HarnessError:
        pass
    # Before anything else: a controller lost by the previous test is rebuilt
    # here, so its casualty list is one test long.
    dev.ensure_radio()
    host.ensure_radio()
    dev.unbond_all()
    host.unbond_all()
    host.start_agent()


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------

def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    set_rundir(os.environ.get("RUNDIR", "/tmp/blooter-twovm"))

    try:
        require_free_port(HUB_PORT, "the hub")
        radio = start_radio_server()
    except HarnessError as exc:
        print(f"\nSETUP FAILED: {exc}", file=sys.stderr, flush=True)
        return 2

    hub = Hub()
    vms = []
    try:
        vms = [boot_vm("dev", hub.port), boot_vm("host", hub.port)]
        dev = DevVm(hub.await_agent("dev"))
        host = HostVm(hub.await_agent("host"))

        # Connection order is identity (§2.3): dev first, confirmed up, then
        # host. Nothing else in the run may connect to the radio.
        dev.start_radio()
        host.start_radio()
        if dev.address == host.address:
            raise HarnessError(
                f"both VMs report {dev.address}; btvirt did not assign "
                "distinct addresses, which means they connected out of order")
        host.start_agent()

        import tests as scenarios

        def make_context():
            name = f"t{int(time.monotonic() * 1000)}"
            dev.btmon_start(name)
            host.btmon_start(name)
            return TestContext(dev, host, name)

        return scenarios.tests.run(make_context, only=only,
                                   setup=lambda: reset(dev, host))
    except HarnessError as exc:
        print(f"\nSETUP FAILED: {exc}", file=sys.stderr, flush=True)
        for proc in [radio] + vms:
            print(f"\n--- {proc.name} ---\n"
                  + "\n".join(proc.output().splitlines()[-40:]), flush=True)
        return 2
    finally:
        hub.close()
        for proc in reversed(vms):
            proc.stop(timeout=20.0)
        radio.stop()


if __name__ == "__main__":
    sys.exit(main())
