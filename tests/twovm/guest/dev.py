"""The `dev` guest: blooter, exactly as a user would run it.

Real config file, real transport, real agent, real bluetoothd -- the only thing
arranged for the test's benefit is where the input comes from. Two paths exist
for that, and they cover different things (design/TESTS.md §4.1):

- **FIFO (`-f`)** for most tests. Deterministic, nothing to fabricate, and the
  path `tests/btvirt` already uses. It disables gamepad forwarding and never
  exercises `EVIOCGRAB`.
- **uinput** for the few that need the real thing. A virtual keyboard and mouse
  are created here, blooter is pointed at them with `-e`/`-x`, and events are
  injected through uinput. This is the only way to reach the evdev path, the
  exclusive grab and capture toggling -- all invisible to FIFO mode.

blooter is normally run non-interactively (stdin on /dev/null), as `tests/btvirt`
does. `interactive=True` puts its stdin and stdout on a pty instead, which is
what lets a test press `[u]` or `[f]` and so assert that the remedy blooter
*prints* is one that actually works (§6). That is not terminal testing --
env_logger writes to stderr, which stays on a plain file, so every log assertion
reads exactly what it reads in the non-interactive case. The TUI's rendering
remains `tests/termdbus`'s job.
"""

import fcntl
import glob
import os
import re
import signal
import struct
import time

from base import RoleAgent
from common import (
    EV_KEY,
    EV_REL,
    EV_SYN,
    HarnessError,
    Process,
    PtyProcess,
    SYN_REPORT,
    input_event,
    log,
    parse_input_events,
    rundir,
    wait_for,
)
from common.evdev import INPUT_EVENT

BLOOTER = os.environ.get("BLOOTER", "/usr/bin/blooter")

# uinput ioctls (linux/uinput.h).
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_SET_RELBIT = 0x40045566
# struct uinput_user_dev: char name[80]; struct input_id id; __u32
# ff_effects_max; __s32 abs{max,min,fuzz,flat}[64].
UINPUT_USER_DEV = struct.Struct("@80sHHHHi256i")


class Blooter:
    """The binary under test."""

    def __init__(self, protocol="classic", extra_args=(), config_extra="",
                 batch="none", interactive=False, fifo=True):
        self.protocol = protocol
        self.extra_args = list(extra_args)
        self.config_extra = config_extra
        self.batch = batch
        self.interactive = interactive
        self.fifo_path = os.path.join(rundir(), "blooter.fifo") if fifo else None
        self.config_path = os.path.join(rundir(), "blooter.toml")
        self.log_path = os.path.join(rundir(), "blooter.log")
        self.proc = None
        self._fifo_fd = None
        self._stderr = None

    # -- lifecycle ----------------------------------------------------------

    def write_config(self):
        with open(self.config_path, "w") as fh:
            # The transport is always pinned rather than left to the default, so
            # a change of default cannot silently move a test to the other one.
            fh.write(f'[connection]\nprotocol = "{self.protocol}"\n')
            batch = (self.batch if isinstance(self.batch, int)
                     else f'"{self.batch}"')
            fh.write(f'[pointer]\nbatch = {batch}\n')
            fh.write(self.config_extra)

    def start(self, timeout=40.0):
        if self.fifo_path:
            if os.path.exists(self.fifo_path):
                os.unlink(self.fifo_path)
            os.mkfifo(self.fifo_path, 0o600)
        self.write_config()

        argv = [BLOOTER, "-c", self.config_path, "-d"]
        if self.fifo_path:
            argv += ["-f", self.fifo_path]
        argv += self.extra_args

        env = dict(os.environ, RUST_BACKTRACE="1")
        # The host filesystem is read-only under virtme-ng, so blooter's own
        # state (the per-host descriptor fingerprints of CONNECTION.md §7.1) has
        # to live somewhere writable or every run starts blind.
        env["XDG_STATE_HOME"] = os.path.join(rundir(), "state")
        env["HOME"] = rundir()
        os.makedirs(env["XDG_STATE_HOME"], exist_ok=True)

        if self.interactive:
            # stderr stays off the pty: env_logger writes there, so every log
            # assertion reads the same bytes in both modes, and only the TUI and
            # blooter's own println!s land on the terminal.
            self._stderr = open(self.log_path, "wb")
            self.proc = PtyProcess("blooter-tty", argv, env=env,
                                   stderr_to=self._stderr)
        else:
            # Non-interactive, exactly as tests/btvirt runs it: stdin on
            # /dev/null, which is how blooter infers that there is no menu to
            # show.
            self.proc = Process("blooter", argv, env=env)

        self.wait_for_output(r"ready to accept connections", timeout,
                             "blooter to be ready")

        if self.fifo_path:
            # Open the write end only once blooter is up: its reader blocks in
            # open(2) until a writer appears and reopens on EOF, so this fd is
            # held for the process lifetime to keep one continuous stream.
            self._fifo_fd = os.open(self.fifo_path, os.O_WRONLY)
        return self

    # -- output -------------------------------------------------------------

    def output(self):
        """Everything blooter has said.

        Interactively that is two streams -- the stderr log and the terminal,
        the latter with its escape sequences stripped -- because `println!`
        (including "ready to accept connections") goes to stdout while the log
        goes to stderr.
        """
        parts = [self.proc.output()] if self.proc else []
        if self.interactive:
            parts.append(_read(self.log_path))
        return "\n".join(p for p in parts if p)

    def alive(self):
        return self.proc is not None and self.proc.alive()

    def match_count(self, pattern):
        return len(re.findall(pattern, self.output()))

    def wait_for_output(self, pattern, timeout, what=None, after=0):
        what = what or f"blooter output matching {pattern!r}"

        def check():
            if not self.alive():
                raise HarnessError(
                    f"blooter exited (rc={self.proc.returncode}) while waiting "
                    f"for {what}\n--- blooter output ---\n{self.output()}")
            return self.match_count(pattern) > after

        return wait_for(check, timeout, what)

    # -- input --------------------------------------------------------------

    def send(self, data):
        if self._fifo_fd is None:
            raise HarnessError("blooter is not running in FIFO mode")
        os.write(self._fifo_fd, data)

    def press_menu_key(self, keys):
        if not self.interactive:
            raise HarnessError(
                "blooter was not started interactively; the menu has no input")
        self.proc.write_stdin(keys)

    # -- teardown -----------------------------------------------------------

    def stop(self, timeout=10.0):
        if self._fifo_fd is not None:
            try:
                os.close(self._fifo_fd)
            except OSError:
                pass
            self._fifo_fd = None
        rc = None
        if self.proc is not None:
            # SIGTERM, never SIGKILL: blooter unregisters its agent,
            # advertisement and GATT application on the way out, and skipping
            # that would leave the "stale instance" state of CONNECTION.md §8.1
            # behind for the next test to trip over.
            rc = self.proc.stop(signal.SIGTERM, timeout=timeout)
            self.proc = None
        if self._stderr is not None:
            try:
                self._stderr.close()
            except OSError:
                pass
            self._stderr = None
        if self.fifo_path and os.path.exists(self.fifo_path):
            try:
                os.unlink(self.fifo_path)
            except OSError:
                pass
        return rc


class UinputDevice:
    """A virtual input device, for the tests FIFO mode cannot reach.

    Written against the ioctls directly rather than a library: the guest has
    only the standard library, and this is a keyboard and a mouse, not a general
    evdev binding.
    """

    def __init__(self, name, keys=(), rels=()):
        self.name = name
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_KEY)
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_SYN)
        for code in keys:
            fcntl.ioctl(self.fd, UI_SET_KEYBIT, code)
        if rels:
            fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_REL)
            for code in rels:
                fcntl.ioctl(self.fd, UI_SET_RELBIT, code)
        os.write(self.fd, UINPUT_USER_DEV.pack(
            name.encode()[:79], 0x03, 0x1234, 0x5678, 1, 0, *([0] * 256)))
        fcntl.ioctl(self.fd, UI_DEV_CREATE)
        self.event = wait_for(lambda: find_event_node(name), 10.0,
                              f"the uinput device {name!r} to appear")
        log(f"uinput {name!r} -> {self.event}")

    @property
    def index(self):
        return int(re.search(r"event(\d+)$", self.event).group(1))

    def emit(self, *events):
        os.write(self.fd, b"".join(events) + input_event(EV_SYN, SYN_REPORT, 0))

    def destroy(self):
        try:
            fcntl.ioctl(self.fd, UI_DEV_DESTROY)
        except OSError:
            pass
        os.close(self.fd)


def find_event_node(name):
    for path in sorted(glob.glob("/sys/class/input/event*")):
        got = _read(os.path.join(path, "device", "name")).strip()
        if got == name:
            return "/dev/input/" + os.path.basename(path)
    return None


class DevAgent(RoleAgent):
    role = "dev"
    # `-P input` drops bluetoothd's built-in input plugin, which would otherwise
    # own the HID UUID and make blooter's profile registration fail (see the
    # error text in main.rs::register_profile). The *host* guest deliberately
    # keeps it -- see host.py.
    disable_plugins = ("input",)
    # For the uinput tests (design/TESTS.md §4.1): the evdev path, the exclusive
    # grab and capture toggling all need real input devices to grab.
    modules = ("uinput",)

    def __init__(self):
        super().__init__()
        self.blooter = None
        self.uinput = {}
        self._watch_fd = None
        self.commands.update({
            "start_blooter": self.start_blooter,
            "stop_blooter": self.stop_blooter,
            "blooter_output": self.blooter_output,
            "blooter_alive": self.blooter_alive,
            "blooter_match_count": self.blooter_match_count,
            "wait_blooter_output": self.wait_blooter_output,
            "menu_key": self.menu_key,
            "key": self.key,
            "rel": self.rel,
            "frame": self.frame,
            "remove_bond": self.remove_bond,
            "state_hosts": self.state_hosts,
            "make_uinput": self.make_uinput,
            "uinput_key": self.uinput_key,
            "uinput_rel": self.uinput_rel,
            "uinput_indices": self.uinput_indices,
            "destroy_uinput": self.destroy_uinput,
            "watch_input": self.watch_input,
            "watch_read": self.watch_read,
        })

    # -- blooter ------------------------------------------------------------

    def start_blooter(self, protocol="classic", extra_args=(), config_extra="",
                      batch="none", interactive=False, fifo=True, timeout=40.0):
        self.stop_blooter()
        # Before blooter, not after: the adapter's bearers can only be changed
        # while it is powered down (see GuestStack.set_transport).
        self.stack.set_transport(protocol)
        self.blooter = Blooter(protocol=protocol, extra_args=list(extra_args),
                               config_extra=config_extra, batch=batch,
                               interactive=interactive, fifo=fifo)
        self.blooter.start(timeout=timeout)
        return {"log": self.blooter.log_path}

    def stop_blooter(self):
        if self.blooter is not None:
            rc = self.blooter.stop()
            self.blooter = None
            return rc
        return None

    def _running(self):
        if self.blooter is None:
            raise HarnessError("no blooter is running")
        return self.blooter

    def blooter_output(self, tail=None):
        if self.blooter is None:
            return ""
        out = self.blooter.output()
        return out if tail is None else "\n".join(out.splitlines()[-tail:])

    def blooter_alive(self):
        return self.blooter is not None and self.blooter.alive()

    def blooter_match_count(self, pattern):
        return self._running().match_count(pattern)

    def wait_blooter_output(self, pattern, timeout=15.0, what=None, after=0):
        self._running().wait_for_output(pattern, timeout, what, after=after)
        return self._running().match_count(pattern)

    def menu_key(self, keys):
        self._running().press_menu_key(keys)

    # -- FIFO input ---------------------------------------------------------

    def key(self, code, pressed):
        self._running().send(
            input_event(EV_KEY, code, 1 if pressed else 0)
            + input_event(EV_SYN, SYN_REPORT, 0))

    def rel(self, code, value):
        self._running().send(input_event(EV_REL, code, value)
                             + input_event(EV_SYN, SYN_REPORT, 0))

    def frame(self, events):
        """One input frame carrying several events, as `[[type, code, value],
        ...]`. blooter merges the whole frame into a single report."""
        self._running().send(
            b"".join(input_event(*e) for e in events)
            + input_event(EV_SYN, SYN_REPORT, 0))

    # -- uinput input -------------------------------------------------------

    def make_uinput(self, keyboard_keys, mouse_keys, mouse_rels):
        self.destroy_uinput()
        self.uinput["keyboard"] = UinputDevice(
            "twovm test keyboard", keys=keyboard_keys)
        self.uinput["mouse"] = UinputDevice(
            "twovm test mouse", keys=mouse_keys, rels=mouse_rels)
        return self.uinput_indices()

    def uinput_indices(self):
        return {name: dev.index for name, dev in self.uinput.items()}

    def uinput_key(self, which, code, pressed):
        self.uinput[which].emit(input_event(EV_KEY, code, 1 if pressed else 0))

    def uinput_rel(self, which, code, value):
        self.uinput[which].emit(input_event(EV_REL, code, value))

    def destroy_uinput(self):
        self.watch_input(None)
        for dev in self.uinput.values():
            dev.destroy()
        self.uinput = {}

    def watch_input(self, which):
        """Open one of the uinput devices for reading, as a rival consumer.

        This is how the exclusive grab is asserted: while blooter holds
        `EVIOCGRAB` on a device, nothing injected into it reaches any other
        reader. Release the grab and the same injection arrives here. That is a
        property of the kernel, not of a log line, so it cannot pass by accident.
        """
        if self._watch_fd is not None:
            os.close(self._watch_fd)
            self._watch_fd = None
        if which is None:
            return None
        path = self.uinput[which].event
        self._watch_fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        return path

    def watch_read(self, timeout=1.0, settle=0.2):
        """Every event the rival reader has seen within `timeout`."""
        if self._watch_fd is None:
            raise HarnessError("watch_input was not called")
        events, deadline = [], time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                data = os.read(self._watch_fd, INPUT_EVENT.size * 64)
            except BlockingIOError:
                if events:
                    # Something arrived; give the rest of the frame a moment
                    # rather than returning half of it.
                    deadline = min(deadline, time.monotonic() + settle)
                time.sleep(0.02)
                continue
            except OSError:
                break
            events += parse_input_events(data)
        return events

    # -- bonds and state ----------------------------------------------------

    def remove_bond(self, address):
        """Drop blooter's half of a bond.

        This is what the menu's `[u]` does (CONNECTION.md §7.2b) reduced to its
        effect on bluetoothd. Tests that are asserting the *remedy blooter
        printed* press `[u]` instead, through `menu_key`; this is for the rows
        of §6 that damage a bond store out of band, which the design requires be
        done directly rather than through blooter's own menu.
        """
        return self.shell(["bluetoothctl", "remove", address], timeout=20.0)

    def state_hosts(self):
        """blooter's per-host descriptor fingerprints (CONNECTION.md §7.1)."""
        path = os.path.join(rundir(), "state", "blooter", "hosts")
        return _read(path)

    # -- teardown -----------------------------------------------------------

    def shutdown(self):
        self.stop_blooter()
        self.destroy_uinput()
        super().shutdown()


def _read(path):
    try:
        with open(path, "rb") as fh:
            return fh.read().decode("utf-8", "replace")
    except OSError:
        return ""
