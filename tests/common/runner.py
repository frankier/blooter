"""A minimal test runner: neither guest has pytest.

Beyond running functions and counting failures it knows one thing pytest would
have given us for free and that `tests/twovm` needs (design/TESTS.md §6.1): an
**expected failure**. Assertions that specify unimplemented behaviour are
written now and marked `xfail`, so they document the commitment without turning
the suite red -- and an xfail that starts passing is reported as XPASS, which is
how the work gets noticed as done.
"""

import time
import traceback


class Registry:
    """Collects test functions.

    Usable bare (`@tests.test`) or with arguments (`@tests.test(xfail="...")`).
    """

    def __init__(self):
        self.tests = []

    def test(self, func=None, *, xfail=None):
        def register(f):
            self.tests.append((f, xfail))
            return f

        return register(func) if func is not None else register

    def run(self, make_context, only=None, setup=None):
        """Run every registered test.

        `make_context()` builds the per-test context handed to the test
        function; if it has `dump_diagnostics()` / `cleanup()` those are called
        on failure and afterwards respectively. `setup()`, if given, runs before
        each test and outside the context.
        """
        passed, failed, skipped, xfailed, xpassed = [], [], [], [], []

        for func, xfail in self.tests:
            name = func.__name__
            if only and only not in name:
                skipped.append(name)
                continue
            print(f"\n>>> {name}" + (f"  [xfail: {xfail}]" if xfail else ""),
                  flush=True)
            try:
                if setup is not None:
                    setup()
                ctx = make_context()
            except Exception as exc:  # noqa: BLE001 - one bad reset is one test
                # A setup failure is this test's failure, not the run's: with
                # thirty-odd tests behind it, aborting everything because one
                # per-test reset went wrong loses far more than it protects.
                failed.append((name, exc))
                print(f"<<< FAIL {name} (setup): {type(exc).__name__}: {exc}",
                      flush=True)
                traceback.print_exc()
                continue
            start = time.monotonic()
            try:
                func(ctx)
            except Exception as exc:  # noqa: BLE001 - reported, not swallowed
                elapsed = time.monotonic() - start
                if xfail:
                    xfailed.append((name, exc))
                    print(f"<<< xfail {name} ({elapsed:.1f}s): "
                          f"{type(exc).__name__}: {exc}", flush=True)
                else:
                    failed.append((name, exc))
                    print(f"<<< FAIL {name} ({elapsed:.1f}s): "
                          f"{type(exc).__name__}: {exc}", flush=True)
                    traceback.print_exc()
                    _dump(ctx)
            else:
                elapsed = time.monotonic() - start
                if xfail:
                    # Not a failure: the behaviour it specifies now works. It is
                    # reported loudly so the marker gets removed.
                    xpassed.append(name)
                    print(f"<<< XPASS {name} ({elapsed:.1f}s) -- "
                          f"expected-fail marker can come off", flush=True)
                else:
                    passed.append(name)
                    print(f"<<< pass {name} ({elapsed:.1f}s)", flush=True)
            finally:
                _cleanup(ctx)

        print("\n" + "=" * 60, flush=True)
        summary = [f"passed: {len(passed)}", f"failed: {len(failed)}"]
        if xfailed:
            summary.append(f"xfail: {len(xfailed)}")
        if xpassed:
            summary.append(f"XPASS: {len(xpassed)}")
        if skipped:
            summary.append(f"skipped: {len(skipped)}")
        print("   ".join(summary), flush=True)
        for name, exc in failed:
            print(f"  FAILED {name}: {type(exc).__name__}: {exc}", flush=True)
        for name, exc in xfailed:
            print(f"  xfail  {name}: {exc}", flush=True)
        for name in xpassed:
            print(f"  XPASS  {name} (remove the xfail marker)", flush=True)
        return 1 if failed else 0


def _dump(ctx):
    dump = getattr(ctx, "dump_diagnostics", None)
    if dump is not None:
        try:
            dump()
        except Exception as exc:  # noqa: BLE001 - diagnostics must not mask
            print(f"    | diagnostics failed: {exc}", flush=True)


def _cleanup(ctx):
    clean = getattr(ctx, "cleanup", None)
    if clean is not None:
        try:
            clean()
        except Exception as exc:  # noqa: BLE001 - cleanup must not mask
            print(f"    | cleanup failed: {exc}", flush=True)
