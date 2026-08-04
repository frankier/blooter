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

    def run(self, make_context, only=None, setup=None, repeat=1):
        """Run every registered test, `repeat` times each.

        `make_context()` builds the per-test context handed to the test
        function; if it has `dump_diagnostics()` / `cleanup()` those are called
        on failure and afterwards respectively. `setup()`, if given, runs before
        each iteration and outside the context.

        `repeat` is for pinning down a flake, and is never the default: a row
        that fails 3 times in 5 is a race, a row that fails 5 in 5 is a defect,
        and the two want different work. A row counts as failed if *any* of its
        iterations failed, and the summary says how many, so the distinction
        survives into the one-line-per-row report at the bottom.
        """
        passed, failed, skipped, xfailed, xpassed = [], [], [], [], []

        for func, xfail in self.tests:
            name = func.__name__
            if only and only not in name:
                skipped.append(name)
                continue
            results = []
            for i in range(repeat):
                run_of = f"  (run {i + 1}/{repeat})" if repeat > 1 else ""
                print(f"\n>>> {name}{run_of}"
                      + (f"  [xfail: {xfail}]" if xfail else ""), flush=True)
                results.append(self._run_once(func, xfail, make_context, setup))

            errors = [exc for outcome, exc in results if outcome == "fail"]
            if errors:
                failed.append((name, errors[0], _tally(results, "fail", repeat)))
            elif any(outcome == "xfail" for outcome, _ in results):
                exc = next(e for o, e in results if o == "xfail")
                xfailed.append((name, exc, _tally(results, "xfail", repeat)))
            elif xfail:
                # Not a failure: the behaviour it specifies now works, every
                # time. It is reported loudly so the marker gets removed.
                xpassed.append(name)
            else:
                passed.append(name)

        print("\n" + "=" * 60, flush=True)
        summary = [f"passed: {len(passed)}", f"failed: {len(failed)}"]
        if xfailed:
            summary.append(f"xfail: {len(xfailed)}")
        if xpassed:
            summary.append(f"XPASS: {len(xpassed)}")
        if skipped:
            summary.append(f"skipped: {len(skipped)}")
        print("   ".join(summary), flush=True)
        for name, exc, tally in failed:
            print(f"  FAILED {name}{tally}: {type(exc).__name__}: {exc}",
                  flush=True)
        for name, exc, tally in xfailed:
            print(f"  xfail  {name}{tally}: {exc}", flush=True)
        for name in xpassed:
            print(f"  XPASS  {name} (remove the xfail marker)", flush=True)
        return 1 if failed else 0

    def _run_once(self, func, xfail, make_context, setup):
        """One iteration: set up, run, report, clean up. Never raises."""
        name = func.__name__
        try:
            if setup is not None:
                setup()
            ctx = make_context()
        except Exception as exc:  # noqa: BLE001 - one bad reset is one test
            # A setup failure is this test's failure, not the run's: with
            # thirty-odd tests behind it, aborting everything because one
            # per-test reset went wrong loses far more than it protects.
            print(f"<<< FAIL {name} (setup): {type(exc).__name__}: {exc}",
                  flush=True)
            traceback.print_exc()
            return ("fail", exc)
        start = time.monotonic()
        try:
            func(ctx)
        except Exception as exc:  # noqa: BLE001 - reported, not swallowed
            elapsed = time.monotonic() - start
            if xfail:
                print(f"<<< xfail {name} ({elapsed:.1f}s): "
                      f"{type(exc).__name__}: {exc}", flush=True)
                return ("xfail", exc)
            print(f"<<< FAIL {name} ({elapsed:.1f}s): "
                  f"{type(exc).__name__}: {exc}", flush=True)
            traceback.print_exc()
            _dump(ctx)
            return ("fail", exc)
        else:
            elapsed = time.monotonic() - start
            if xfail:
                print(f"<<< XPASS {name} ({elapsed:.1f}s) -- "
                      f"expected-fail marker can come off", flush=True)
                return ("xpass", None)
            print(f"<<< pass {name} ({elapsed:.1f}s)", flush=True)
            return ("pass", None)
        finally:
            _cleanup(ctx)


def _tally(results, outcome, repeat):
    """" (3/5 runs)", or "" for a single run -- a flake, said in one place."""
    if repeat == 1:
        return ""
    return f" ({sum(1 for o, _ in results if o == outcome)}/{repeat} runs)"


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
