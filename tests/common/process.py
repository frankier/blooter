"""Child processes with captured output, and the polling primitive.

Everything in either suite is asynchronous startup -- a daemon that registers on
a bus, a controller that appears in sysfs, a bond that lands in storage -- so
nothing here ever sleeps for a fixed time. `wait_for` is the only waiting
primitive, and every use of it names what it is waiting for so a timeout says
something.
"""

import fcntl
import os
import pty
import re
import signal
import struct
import subprocess
import termios
import threading
import time

_RUNDIR = "/tmp/blooter"

# Enough to strip a TUI's drawing out of a transcript. Not a terminal emulator:
# the suites that use this assert on *lines a program printed*, never on what a
# screen would look like, which is `tests/termdbus`'s job.
ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][B0]|\x1b[=>]|\r")


class HarnessError(Exception):
    """Setup failed -- distinct from a test assertion failing."""


def set_rundir(path):
    """Where component logs and FIFOs are written. Each suite picks its own so
    two of them running back to back cannot read each other's logs."""
    global _RUNDIR
    _RUNDIR = path
    os.makedirs(path, exist_ok=True)
    return path


def rundir():
    return _RUNDIR


def log(msg):
    print(f"    | {msg}", flush=True)


def wait_for(predicate, timeout, what, interval=0.05):
    """Poll until `predicate()` is truthy, returning its value."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise HarnessError(f"timed out after {timeout}s waiting for {what}")


class Process:
    """A child process with its output captured to a file, so a failure can
    show what the component actually said."""

    def __init__(self, name, argv, env=None, stdin_pipe=False, cwd=None,
                 logdir=None):
        self.name = name
        self.argv = argv
        self.log_path = os.path.join(logdir or _RUNDIR, f"{name}.log")
        os.makedirs(os.path.dirname(self.log_path), exist_ok=True)
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            argv,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            stdin=subprocess.PIPE if stdin_pipe else subprocess.DEVNULL,
            env=env,
            cwd=cwd,
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


class PtyProcess(Process):
    """A child on a pty, for programs that behave differently off a terminal.

    Two of them need it: `bluetoothctl`, which block-buffers its output down a
    pipe so a scripted conversation with it goes nowhere, and blooter itself when
    a test needs to press a menu key. Output is recorded with escape sequences
    stripped, so `wait_for_output` works exactly as it does for a plain pipe.

    `stderr_to` keeps a separate stream off the pty -- blooter's log lines go to
    stderr, and keeping them on a plain file means every log assertion reads the
    same bytes whether or not the process was given a terminal.
    """

    def __init__(self, name, argv, env=None, logdir=None, stderr_to=None,
                 rows=40, cols=100):
        self.name = name
        self.argv = argv
        self.log_path = os.path.join(logdir or _RUNDIR, f"{name}.log")
        os.makedirs(os.path.dirname(self.log_path), exist_ok=True)
        self._log = open(self.log_path, "wb")
        master, slave = pty.openpty()
        # crossterm asks the terminal for its size and draws nothing sensible
        # into a 0x0 one, so give the pty a real window.
        fcntl.ioctl(slave, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen(
            argv, stdin=slave, stdout=slave,
            stderr=stderr_to if stderr_to is not None else slave,
            env=env, start_new_session=True)
        os.close(slave)
        self.master = master
        # Drained by a thread, continuously, rather than on demand before each
        # read. A pty's buffer is small and a TUI redraws constantly: leave it
        # to fill and the child *blocks in write* -- which looks like a program
        # that has stopped responding to input, not like a full buffer, and cost
        # a long afternoon of blaming the keystrokes.
        self._lock = threading.Lock()
        self._reader = threading.Thread(target=self._pump, daemon=True)
        self._reader.start()

    def _pump(self):
        while True:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            with self._lock:
                if self._log.closed:
                    return
                self._log.write(chunk)
                self._log.flush()

    def output(self):
        with self._lock:
            return ANSI.sub("", super().output())

    def write_stdin(self, text):
        os.write(self.master, text.encode())

    def stop(self, sig=signal.SIGTERM, timeout=5.0):
        rc = super().stop(sig=sig, timeout=timeout)
        if self.master is not None:
            try:
                os.close(self.master)
            except OSError:
                pass
            self.master = None
        return rc
