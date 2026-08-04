"""The scenarios of design/TESTS.md §5 and §6.

Host-side Python throughout: every test issues commands to the two VMs through
the hub, so a scenario spanning two machines still reads top to bottom.

Most rows run on **both** transports, because almost everything here behaves
differently on each -- a bond is a link key on Classic and an LTK on BLE, a
reconnect is a dial on one and an advertisement on the other, and half the point
of §6 is which of the two a given divergence breaks.
"""

from common import Registry

tests = Registry()


def both_transports(func=None, *, xfail=None):
    """Register one test per transport, as `<name>_classic` and `<name>_ble`.

    The transport arrives as the second argument. Registering two functions
    rather than looping inside one is deliberate: a Classic failure and a BLE
    failure are different findings, and a suite that reports them as one row
    hides whichever came second.
    """
    def register(f):
        for protocol in ("classic", "ble"):
            def variant(t, _f=f, _p=protocol):
                _f(t, _p)
            variant.__name__ = f"{f.__name__}_{protocol}"
            variant.__doc__ = f.__doc__
            tests.test(variant, xfail=xfail)
        return f

    return register(func) if func is not None else register


from . import test_journeys  # noqa: E402,F401  (registers on import)
from . import test_divergence  # noqa: E402,F401


def _classic_before_ble(entry):
    """Sort key: everything Classic, then everything BLE.

    Not cosmetic. Switching the dev adapter between BR/EDR and LE-only is a
    power cycle of an emulated controller that reaches this machine over TCP,
    and doing it between every pair of tests has been seen to lose the
    controller outright. Grouping the transports makes it happen about twice a
    run instead of thirty times. Relative order within each group is preserved,
    so J1 still comes before J2.
    """
    return 1 if "_ble" in entry[0].__name__ else 0


tests.tests.sort(key=_classic_before_ble)
