"""Terminal + D-Bus test harness for blooter's interactive menu and pairing prompt.

Complements tests/btvirt, which exercises the real L2CAP link but must run
blooter non-interactively. This harness inverts that: BlueZ is mocked
(python-dbusmock) so devices and pairing requests can be scripted exactly, and
blooter runs under a PTY (termwright) so the menu and the TTY prompt are real
and can be asserted on character by character.

    python-dbusmock (org.bluez)  <--D-Bus--  blooter  --PTY-->  termwright
        scripted devices,                                       screen text,
        agent calls                                             keystrokes

What is real here: blooter itself, its crossterm rendering, its raw-mode
handling, and the TermCoord hand-off between menu and pairing prompt. What is
mocked: the Bluetooth stack below D-Bus. Nothing here touches a controller.

Runs inside the virtme-ng guest as root, because blooter binds L2CAP PSMs
0x11/0x13 which need CAP_NET_BIND_SERVICE. No controller is required for that
bind, so this harness starts no btvirt and no bluetoothd.
"""

import json
import os
import re
import signal
import socket
import subprocess
import time
import traceback

RUNDIR = "/tmp/blooter-termdbus"
BUS_SOCKET = "/run/dbus/termdbus_bus_socket"
BUS_ADDRESS = f"unix:path={BUS_SOCKET}"

DBUS_DAEMON = "/usr/bin/dbus-daemon"

ADAPTER = "hci0"
ADAPTER_PATH = f"/org/bluez/{ADAPTER}"
# dbusmock's bluez5 template puts AgentManager1/ProfileManager1 here.
AGENT_MANAGER_PATH = "/org/bluez"

# Class-of-device values used to drive the "Other devices" split (menu.rs::is_other).
CLASS_COMPUTER = 0x00010C  # major class 1 (computer) -- stays in the main list
CLASS_HEADSET = 0x240404  # major class 4 (audio/video) -- goes to "Other devices"
CLASS_TV = 0x24043C  # major 4 / minor 0x0F (display) -- stays in the main list
CLASS_COMPUTER_AUDIO = 0x20010C  # a computer advertising A2DP -- stays in the main list

# GAP Appearance values, the LE counterpart of the classes above: LE-only peers
# carry no Class of Device, so menu.rs::is_other falls through to these.
APPEARANCE_COMPUTER = 0x0080  # category 0x02 -- stays in the main list
APPEARANCE_KEYBOARD = 0x03C1  # category 0x0F (HID) -- goes to "Other devices"
APPEARANCE_SPEAKER = 0x0841  # category 0x21 (audio sink) -- goes to "Other devices"


class HarnessError(Exception):
    """Setup failed -- distinct from a test assertion failing."""


def log(msg):
    print(f"    | {msg}", flush=True)


def wait_for(predicate, timeout, what, interval=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise HarnessError(f"timed out after {timeout}s waiting for {what}")


def device_path(address):
    return f"{ADAPTER_PATH}/dev_" + address.replace(":", "_").upper()


# --------------------------------------------------------------------------
# Processes
# --------------------------------------------------------------------------

class Process:
    def __init__(self, name, argv, env=None):
        self.name = name
        self.log_path = os.path.join(RUNDIR, f"{name}.log")
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            argv, stdout=self._log, stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL, env=env, start_new_session=True)

    def output(self):
        try:
            with open(self.log_path, "rb") as fh:
                return fh.read().decode("utf-8", "replace")
        except OSError:
            return ""

    def alive(self):
        return self.proc.poll() is None

    def stop(self, timeout=5.0):
        if self.proc.poll() is None:
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
                self.proc.wait(timeout=timeout)
        try:
            self._log.close()
        except OSError:
            pass
        return self.proc.returncode


# --------------------------------------------------------------------------
# Mocked BlueZ
# --------------------------------------------------------------------------

class MockBluez:
    """A private system bus running python-dbusmock's bluez5 template.

    Gives complete control over what blooter sees: which devices exist, their
    class and name (which decide the main/"Other devices" split), whether they
    are paired, and -- crucially -- when a pairing request arrives, which is what
    the TermCoord hand-off is about.
    """

    def __init__(self):
        self.procs = []
        self.bus = None
        self._mock = None

    def start(self):
        os.makedirs(RUNDIR, exist_ok=True)
        self._start_bus()
        self._start_mock()
        self._connect()
        self.add_adapter()

    def _start_bus(self):
        os.makedirs("/run/dbus", exist_ok=True)
        if os.path.exists(BUS_SOCKET):
            os.unlink(BUS_SOCKET)
        config = os.path.join(RUNDIR, "bus.conf")
        with open(config, "w") as fh:
            fh.write(f"""<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path={BUS_SOCKET}</listen>
  <policy context="default">
    <allow user="*"/>
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
""")
        log("starting private system bus")
        self.procs.append(Process("dbus", [DBUS_DAEMON, "--config-file", config,
                                           "--nofork", "--nopidfile"]))
        wait_for(lambda: os.path.exists(BUS_SOCKET), 10.0, "the bus socket")
        os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = BUS_ADDRESS

    def _start_mock(self):
        log("starting dbusmock bluez5 template")
        env = dict(os.environ, DBUS_SYSTEM_BUS_ADDRESS=BUS_ADDRESS)
        self.procs.append(Process(
            "dbusmock", ["python3", "-m", "dbusmock", "--template", "bluez5"],
            env=env))

    def _connect(self):
        import dbus

        self.bus = dbus.bus.BusConnection(BUS_ADDRESS)

        def owned():
            try:
                return self.bus.name_has_owner("org.bluez")
            except Exception:  # noqa: BLE001 - bus may not be ready yet
                return False

        wait_for(owned, 15.0, "org.bluez to appear on the mock bus")
        self._mock = dbus.Interface(
            self.bus.get_object("org.bluez", "/"), "org.bluez.Mock")

    # -- object helpers -----------------------------------------------------

    def mock_iface(self, path, interface="org.freedesktop.DBus.Mock"):
        import dbus
        return dbus.Interface(self.bus.get_object("org.bluez", path), interface)

    def add_adapter(self):
        self._mock.AddAdapter(ADAPTER, "blooter-test")
        # The bluez5 template serves LEAdvertisingManager1 but no GattManager1,
        # which blooter's LE transport needs to register its HOGP tree. Stub it:
        # accepting the registration is all the menu tests need.
        self.mock_iface(ADAPTER_PATH).AddMethods("org.bluez.GattManager1", [
            ("RegisterApplication", "oa{sv}", "", ""),
            ("UnregisterApplication", "o", "", ""),
        ])
        log(f"added adapter {ADAPTER}")

    def add_device(self, address, alias, cls=CLASS_COMPUTER, paired=False,
                   named=True, rssi=-60, appearance=None):
        """Add a discoverable device.

        `named=False` *removes* the Name property rather than blanking it:
        blooter's split keys off the property being absent
        (`dev.name().ok().flatten().is_some()`), and an empty string would still
        count as a real name.

        `cls=None` likewise removes Class, which is what an LE-only peer looks
        like; pass `appearance=` to drive the split from GAP Appearance instead.
        """
        import dbus

        self._mock.AddDevice(ADAPTER, address, alias)
        path = device_path(address)
        props = self.mock_iface(path, "org.freedesktop.DBus.Properties")
        dev = self.mock_iface(path)

        updates = {
            "RSSI": dbus.Int16(rssi),
            "Alias": dbus.String(alias),
        }
        if cls is not None:
            updates["Class"] = dbus.UInt32(cls)
        if appearance is not None:
            updates["Appearance"] = dbus.UInt16(appearance)
        dev.UpdateProperties("org.bluez.Device1", updates)
        # The template gives every device a phone Class; an LE-only peer has
        # none, so `cls=None` deletes it the same way `named=False` deletes Name.
        if cls is None:
            self.drop_property(path, "org.bluez.Device1", "Class")
        if paired:
            self._mock.PairDevice(ADAPTER, address)
        if not named:
            self.drop_property(path, "org.bluez.Device1", "Name")
        return path

    def calls(self, path):
        """The mock's recorded method calls on `path`, as a list of names.

        Used to assert that a menu pick actually reached BlueZ (Pair, Connect).
        """
        return [str(name) for _ts, name, _args in self.mock_iface(path).GetCalls()]

    def drop_property(self, path, interface, name):
        """Delete a property outright.

        dbusmock has no D-Bus call for this, but it can execute Python in the
        mock process, which is exactly what AddMethod is for.
        """
        self.mock_iface(path).AddMethod(
            "org.bluez.Mock", "DropProp", "ss", "",
            "del self.props[args[0]][args[1]]")
        self.mock_iface(path, "org.bluez.Mock").DropProp(interface, name)

    def remove_devices(self):
        """Clear the device list between tests."""
        import dbus

        manager = dbus.Interface(self.bus.get_object("org.bluez", "/"),
                                 "org.freedesktop.DBus.ObjectManager")
        remover = self.mock_iface("/")  # org.freedesktop.DBus.Mock
        for path, ifaces in list(manager.GetManagedObjects().items()):
            if "org.bluez.Device1" in ifaces:
                try:
                    remover.RemoveObject(path)
                except Exception:  # noqa: BLE001 - already gone is fine
                    pass

    # -- agent --------------------------------------------------------------

    def agent_path(self, timeout=10.0):
        """The agent path blooter registered, read back from the mock's own
        record of the RegisterAgent call."""
        manager = self.mock_iface(AGENT_MANAGER_PATH)

        def registered():
            calls = manager.GetMethodCalls("RegisterAgent")
            return str(calls[-1][1][0]) if calls else None

        return wait_for(registered, timeout, "blooter to register its agent")

    def _agent_owner(self, path):
        """Find the bus name that exports `path`.

        Unique names are not discoverable by object path, so this introspects
        each connection on the (small, private) bus and picks the one that
        exports an Agent1 there.

        Two names are skipped deliberately. Our own connection would deadlock:
        a synchronous call to ourselves cannot be answered while we block
        waiting for the reply, so it costs a full D-Bus timeout. The mock owns
        org.bluez and never exports an agent. Everything else gets a short
        timeout so one unresponsive peer cannot stall the suite.
        """
        import dbus

        skip = {str(self.bus.get_unique_name())}
        try:
            skip.add(str(self.bus.get_name_owner("org.bluez")))
        except Exception:  # noqa: BLE001 - not yet owned; nothing to skip
            pass

        for name in self.bus.list_names():
            name = str(name)
            if not name.startswith(":") or name in skip:
                continue
            try:
                data = dbus.Interface(
                    self.bus.get_object(name, path),
                    "org.freedesktop.DBus.Introspectable").Introspect(timeout=5.0)
            except Exception:  # noqa: BLE001 - wrong owner, keep looking
                continue
            if "org.bluez.Agent1" in str(data):
                return name
        raise HarnessError(f"no bus name exports an Agent1 at {path}")

    def request_confirmation(self, address, passkey=123456):
        """Ask blooter's agent to confirm a pairing, as bluetoothd would for an
        incoming Just Works pair."""
        import dbus
        return self.call_agent("RequestConfirmation",
                               dbus.ObjectPath(device_path(address)),
                               dbus.UInt32(passkey))

    def request_authorization(self, address):
        """Ask blooter's agent to authorize a pairing (no passkey shown)."""
        import dbus
        return self.call_agent("RequestAuthorization",
                               dbus.ObjectPath(device_path(address)))

    def call_agent(self, method, *args, timeout=30):
        """Invoke a method on blooter's pairing agent, as bluetoothd would.

        Returns a handle whose `.accepted()` blocks until blooter answers; the
        call runs on its own thread because blooter will not reply until the
        prompt is answered on the terminal, which the test does next.
        """
        path = self.agent_path()
        owner = self._agent_owner(path)
        return AgentCall(BUS_ADDRESS, owner, path, method, args, timeout)

    def stop(self):
        for proc in reversed(self.procs):
            proc.stop()
        self.procs = []

    def dump_logs(self):
        for proc in self.procs:
            out = proc.output().strip()
            if out:
                print(f"\n--- {proc.name} ---", flush=True)
                print("\n".join(out.splitlines()[-25:]), flush=True)


class AgentCall:
    """An in-flight agent method call, answered later from the terminal.

    The call is made synchronously on its own thread with its own bus
    connection. blooter will not reply until the prompt is answered, and the
    test has to keep driving the terminal in the meantime -- a background thread
    expresses that directly, without needing a main loop to pump async replies.
    """

    def __init__(self, bus_address, owner, path, method, args, timeout):
        import threading

        self._done = {}
        self._method = method

        def run():
            conn = None
            try:
                import dbus
                # A per-thread connection: dbus-python connections are not
                # safe to share across threads.
                conn = dbus.bus.BusConnection(bus_address)
                agent = dbus.Interface(conn.get_object(owner, path),
                                       "org.bluez.Agent1")
                getattr(agent, method)(*args, timeout=timeout)
                self._done["ok"] = True
            except Exception as exc:  # noqa: BLE001 - reported via error_name()
                self._done["err"] = exc
            finally:
                # Leaving it open would strand an idle name on the bus, and the
                # next _agent_owner scan pays a full probe timeout for each one.
                if conn is not None:
                    try:
                        conn.close()
                    except Exception:  # noqa: BLE001 - already gone
                        pass

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()

    def settled(self):
        return bool(self._done)

    def accepted(self, timeout=15.0):
        """True if blooter accepted the request, False if it rejected."""
        self._thread.join(timeout)
        if not self._done:
            raise AssertionError(
                f"blooter never answered {self._method} within {timeout}s")
        return "ok" in self._done

    def error_name(self):
        exc = self._done.get("err")
        return exc.get_dbus_name() if hasattr(exc, "get_dbus_name") else None


# --------------------------------------------------------------------------
# Terminal (termwright daemon)
# --------------------------------------------------------------------------

class Term:
    """A TUI session driven through termwright's daemon socket.

    termwright wraps the process in a PTY, which is the point: blooter decides
    it is interactive with `isatty(stdin)`, and only then does it run the menu
    and prompt in `confirm` mode.
    """

    def __init__(self, name, argv, env=None, cols=100, rows=30):
        self.socket_path = os.path.join(RUNDIR, f"{name}.sock")
        self._id = 0
        if os.path.exists(self.socket_path):
            os.unlink(self.socket_path)
        self.proc = Process(f"termwright-{name}", [
            "termwright", "daemon", "--socket", self.socket_path,
            "--cols", str(cols), "--rows", str(rows), "--"] + argv, env=env)
        wait_for(lambda: os.path.exists(self.socket_path), 15.0,
                 "the termwright daemon socket")

    def _call(self, method, params=None, timeout=20.0):
        self._id += 1
        request = json.dumps({"id": self._id, "method": method,
                              "params": params}) + "\n"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(timeout)
            sock.connect(self.socket_path)
            sock.sendall(request.encode())
            chunks = []
            while True:
                data = sock.recv(65536)
                if not data:
                    break
                chunks.append(data)
                if b"\n" in data:
                    break
        raw = b"".join(chunks).decode("utf-8", "replace").strip()
        if not raw:
            raise HarnessError(f"empty reply to {method}")
        reply = json.loads(raw.splitlines()[0])
        if reply.get("error"):
            raise HarnessError(f"{method} failed: {reply['error']}")
        return reply.get("result")

    def screen(self):
        result = self._call("screen", {"format": "text"})
        return result if isinstance(result, str) else json.dumps(result)

    def press(self, key):
        self._call("press", {"key": key})

    def type(self, text):
        self._call("type", {"text": text})

    def wait_for_text(self, text, timeout_ms=10000):
        try:
            self._call("wait_for_text", {"text": text, "timeout_ms": timeout_ms},
                       timeout=timeout_ms / 1000.0 + 10)
        except HarnessError as exc:
            raise AssertionError(
                f"never saw {text!r} on screen ({exc})\n"
                f"--- screen ---\n{self.screen()}") from None

    def wait_for_idle(self, idle_ms=300, timeout_ms=5000):
        self._call("wait_for_idle", {"idle_ms": idle_ms, "timeout_ms": timeout_ms},
                   timeout=timeout_ms / 1000.0 + 10)

    def running(self):
        try:
            return bool(self._call("status"))
        except HarnessError:
            return False

    def close(self):
        try:
            self._call("close", None, timeout=5.0)
        except (HarnessError, OSError):
            pass
        self.proc.stop()
        if os.path.exists(self.socket_path):
            try:
                os.unlink(self.socket_path)
            except OSError:
                pass


class Blooter:
    """blooter running under a PTY, in FIFO input mode.

    FIFO mode keeps evdev and udev out of it; the menu, not input, is what these
    tests are about. Bluetooth registration is left on -- the mock serves
    ProfileManager1 for Classic and GattManager1/LEAdvertisingManager1 for BLE --
    so blooter's startup path runs in full either way.

    The transport is always pinned with a written config file rather than left
    to the default, so a change of default cannot silently move a test from one
    transport to the other. `pairing` is written the same way when a test needs
    the TTY prompt (the default is "accept", which never prompts).
    """

    def __init__(self, extra_args=(), protocol="classic", pairing=None):
        self.fifo_path = os.path.join(RUNDIR, "blooter.fifo")
        if os.path.exists(self.fifo_path):
            os.unlink(self.fifo_path)
        os.mkfifo(self.fifo_path, 0o600)
        self.config_path = os.path.join(RUNDIR, "blooter.toml")
        with open(self.config_path, "w") as fh:
            fh.write(f'[connection]\nprotocol = "{protocol}"\n')
            if pairing is not None:
                fh.write(f'pairing = "{pairing}"\n')
        env = dict(os.environ,
                   DBUS_SYSTEM_BUS_ADDRESS=BUS_ADDRESS,
                   RUST_BACKTRACE="1")
        argv = [os.environ["BLOOTER"], "-f", self.fifo_path,
                "-c", self.config_path, "-d"] + list(extra_args)
        self.term = Term("blooter", argv, env=env)
        self._fifo_fd = None

    def open_input(self):
        if self._fifo_fd is None:
            self._fifo_fd = os.open(self.fifo_path, os.O_WRONLY | os.O_NONBLOCK)
        return self._fifo_fd

    def screen(self):
        return self.term.screen()

    def close(self):
        if self._fifo_fd is not None:
            try:
                os.close(self._fifo_fd)
            except OSError:
                pass
        self.term.close()
        if os.path.exists(self.fifo_path):
            try:
                os.unlink(self.fifo_path)
            except OSError:
                pass


# --------------------------------------------------------------------------
# Assertions
# --------------------------------------------------------------------------

def assert_screen_contains(term, needle, what):
    screen = term.screen()
    if needle not in screen:
        raise AssertionError(
            f"{what}\n  expected to find: {needle!r}\n--- screen ---\n{screen}")
    return screen


def assert_screen_lacks(term, needle, what):
    screen = term.screen()
    if needle in screen:
        raise AssertionError(
            f"{what}\n  did not expect: {needle!r}\n--- screen ---\n{screen}")
    return screen


def assert_menu_contains(term, needle, what):
    """Like `assert_screen_contains`, but only against the *current* menu
    render -- an earlier render scrolled above it must not satisfy the check."""
    screen = term.screen()
    block = "\n".join(last_menu_block(screen))
    if needle not in block:
        raise AssertionError(
            f"{what}\n  expected in the current menu: {needle!r}"
            f"\n--- current menu ---\n{block}\n--- full screen ---\n{screen}")
    return block


def assert_menu_lacks(term, needle, what):
    """The current menu render must not contain `needle`."""
    screen = term.screen()
    block = "\n".join(last_menu_block(screen))
    if needle in block:
        raise AssertionError(
            f"{what}\n  did not expect in the current menu: {needle!r}"
            f"\n--- current menu ---\n{block}")
    return block


def wait_for_menu(term, title, timeout=10.0):
    """Wait until `title` is the menu currently showing.

    Not the same as waiting for the text to appear anywhere: after switching
    submenus the previous title is still on screen above the current render.
    """
    try:
        wait_for(lambda: menu_title(term.screen()) == title, timeout,
                 f"the {title!r} menu")
    except HarnessError:
        raise AssertionError(
            f"the current menu never became {title!r} "
            f"(it is {menu_title(term.screen())!r})"
            f"\n--- screen ---\n{term.screen()}") from None


MENU_TITLES = ("Bluetooth hosts:", "Other devices:")


def last_menu_block(screen):
    """The most recent menu render on screen, as a list of lines.

    The menu repaints by moving up and clearing, but when it sits near the
    bottom of the terminal the scroll leaves earlier renders visible above the
    current one. Everything that asks "what does the menu say *now*" therefore
    has to read the last block, not the first match on screen.
    """
    lines = screen.splitlines()
    start = None
    for i, line in enumerate(lines):
        if line.strip() in MENU_TITLES:
            start = i
    return lines[start:] if start is not None else lines


def menu_title(screen):
    """Which menu screen is currently showing, or None."""
    block = last_menu_block(screen)
    return block[0].strip() if block and block[0].strip() in MENU_TITLES else None


def selected_line(screen):
    """The menu row the cursor is on ('>' marker, menu.rs::render_lines)."""
    for line in last_menu_block(screen):
        if line.strip().startswith(">"):
            return line.strip()
    return None


def selected_index(screen):
    """The 1-based number of the row the cursor is on.

    Tests key off this rather than the device name: blooter lists devices in
    whatever order they come back over D-Bus, which is not insertion order.
    """
    line = selected_line(screen)
    if line is None:
        return None
    match = re.match(r">\s*(\d+)\.", line)
    return int(match.group(1)) if match else None


def menu_rows(screen):
    """The current menu's rows as (index, text), cursor marker stripped."""
    rows = []
    for line in last_menu_block(screen):
        match = re.match(r"\s*[>\s]\s*(\d+)\.\s+(.*)", line)
        if match:
            rows.append((int(match.group(1)), match.group(2).strip()))
    return rows


# --------------------------------------------------------------------------
# Minimal test runner
# --------------------------------------------------------------------------

class Registry:
    def __init__(self):
        self.tests = []

    def test(self, func):
        self.tests.append(func)
        return func

    def run(self, mock, only=None):
        passed, failed, skipped = [], [], []
        for func in self.tests:
            name = func.__name__
            if only and only not in name:
                skipped.append(name)
                continue
            print(f"\n>>> {name}", flush=True)
            mock.remove_devices()
            ctx = TestContext(mock)
            start = time.monotonic()
            try:
                func(ctx)
            except Exception as exc:  # noqa: BLE001 - reported, not swallowed
                failed.append((name, exc))
                print(f"<<< FAIL {name} ({time.monotonic() - start:.1f}s): "
                      f"{type(exc).__name__}: {exc}", flush=True)
                traceback.print_exc()
                ctx.dump_diagnostics()
            else:
                passed.append(name)
                print(f"<<< pass {name} ({time.monotonic() - start:.1f}s)",
                      flush=True)
            finally:
                ctx.cleanup()

        print("\n" + "=" * 60, flush=True)
        print(f"passed: {len(passed)}   failed: {len(failed)}"
              + (f"   skipped: {len(skipped)}" if skipped else ""), flush=True)
        for name, exc in failed:
            print(f"  FAILED {name}: {type(exc).__name__}: {exc}", flush=True)
        return 1 if failed else 0


class TestContext:
    def __init__(self, mock):
        self.mock = mock
        self.blooter = None

    def start_blooter(self, extra_args=(), protocol="classic", pairing=None):
        self.blooter = Blooter(extra_args=extra_args, protocol=protocol,
                               pairing=pairing)
        return self.blooter

    def menu(self, extra_args=(), protocol="classic", pairing=None):
        """Start blooter and wait for the host menu to be on screen."""
        blooter = self.start_blooter(extra_args, protocol=protocol,
                                     pairing=pairing)
        blooter.term.wait_for_text("Bluetooth hosts:")
        blooter.term.wait_for_idle()
        return blooter.term

    def dump_diagnostics(self):
        if self.blooter:
            try:
                print("\n--- terminal ---", flush=True)
                print(self.blooter.screen(), flush=True)
            except Exception:  # noqa: BLE001 - daemon may be gone
                pass
            out = self.blooter.term.proc.output().strip()
            if out:
                print("\n--- blooter/termwright output (tail) ---", flush=True)
                print("\n".join(out.splitlines()[-30:]), flush=True)
        self.mock.dump_logs()

    def cleanup(self):
        if self.blooter:
            self.blooter.close()
            self.blooter = None
