#!/usr/bin/env bash
# Menu and pairing-prompt tests for blooter.
#
# Builds blooter on the host, then runs the suite inside a virtme-ng VM. See
# README.md for why the VM is needed even though BlueZ is mocked here.
#
#   ./run.sh              # whole suite
#   ./run.sh pairing      # only tests whose name contains "pairing"
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FILTER="${1:-}"

missing=0
if ! command -v vng >/dev/null 2>&1; then
    echo "virtme-ng (vng) not found:  sudo dnf install virtme-ng" >&2
    echo "                       or:  pipx install virtme-ng" >&2
    missing=1
fi
if ! command -v termwright >/dev/null 2>&1; then
    cat >&2 <<'EOF'
termwright not found. It drives blooter under a PTY.

  cargo install termwright

It needs fontconfig headers to build (for a screenshot feature these tests do
not use):  sudo dnf install fontconfig-devel
EOF
    missing=1
fi
if ! python3 -c "import dbusmock" 2>/dev/null; then
    cat >&2 <<'EOF'
python-dbusmock not found. It provides the mocked org.bluez.

  pip install --user python-dbusmock
  # or: sudo dnf install python3-dbusmock
EOF
    missing=1
fi
((missing)) && exit 2

echo "##### 1/2  building blooter #####"
cargo build --manifest-path "$ROOT/Cargo.toml"
BLOOTER="$ROOT/target/debug/blooter"
[[ -x "$BLOOTER" ]] || { echo "blooter binary missing: $BLOOTER" >&2; exit 2; }

echo
echo "##### 2/2  running tests in VM #####"
# Inside the guest we run as root, so a `pip install --user` dbusmock in the
# invoking user's home is not on the default path -- pass its location through
# explicitly. Same for PATH, which is how termwright (in ~/.cargo/bin) resolves.
USERSITE="$(python3 -c 'import site; print(site.getusersitepackages())' 2>/dev/null || true)"
GUEST_PYTHONPATH="${PYTHONPATH:-}"
[[ -n "$USERSITE" ]] && GUEST_PYTHONPATH="$USERSITE${GUEST_PYTHONPATH:+:$GUEST_PYTHONPATH}"

# -r takes $VNG_KERNEL (a version in /boot) when set, else the host kernel.
vng -r ${VNG_KERNEL:+"$VNG_KERNEL"} --user root \
    -e "BLOOTER='$BLOOTER' FILTER='$FILTER' PATH='$PATH' \
        PYTHONPATH='$GUEST_PYTHONPATH' $HERE/guest/run-tests.sh"
