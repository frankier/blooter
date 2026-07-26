#!/usr/bin/env bash
# Build btvirt and btmgmt from bluez source. No root required -- nothing is
# installed, we just want the two binaries.
#
# btvirt is the emulator, which Fedora's bluez package does not ship (Debian has
# it in bluez-test-tools; Fedora has no equivalent). btmgmt *is* packaged
# everywhere, but the harness drives it non-interactively and older ones
# (5.72, which is what Ubuntu 24.04 ships) never exit when they are: every call
# hangs until the harness timeout. Building it here pins the whole stack to one
# bluez version instead of whatever the distro happens to carry.
set -euo pipefail

VERSION="${BLUEZ_VERSION:-5.87}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/build"
SRC="$BUILD/bluez-$VERSION"
TARBALL="bluez-$VERSION.tar.xz"
URL="https://mirrors.edge.kernel.org/pub/linux/bluetooth/$TARBALL"

if [[ -x "$SRC/emulator/btvirt" && -x "$SRC/tools/btmgmt" ]]; then
    echo "btvirt and btmgmt already built in: $SRC"
    exit 0
fi

# btvirt links against glib and dbus even though we disable most of bluez;
# btmgmt needs readline (bluez only builds it at all when readline is found).
missing=()
for dep in glib-2.0 dbus-1 readline; do
    pkg-config --exists "$dep" || missing+=("$dep")
done
if ((${#missing[@]})); then
    echo "Missing build dependencies: ${missing[*]}" >&2
    echo "On Fedora:  sudo dnf install glib2-devel dbus-devel readline-devel" >&2
    exit 1
fi

mkdir -p "$BUILD"
cd "$BUILD"

if [[ ! -f "$TARBALL" ]]; then
    echo "==> fetching $URL"
    curl -fL --retry 3 --retry-connrefused -O "$URL"
fi

if [[ ! -d "$SRC" ]]; then
    echo "==> extracting"
    tar xf "$TARBALL"
fi

cd "$SRC"
# Unconditional: we only get here with a binary missing, so we are building
# anyway, and a tree configured before readline was installed would otherwise
# keep silently omitting btmgmt's sources (it links, and fails, with no main).
echo "==> configuring (--enable-testing is what builds emulator/btvirt)"
# Everything not needed for the two binaries is disabled: this keeps the
# dependency surface small and the build to well under a minute. The client
# stays enabled only because bluez gates its readline check -- and so the whole
# READLINE conditional, and with it btmgmt's sources -- on it; nothing in
# client/ is actually built, since make is given explicit targets.
./configure \
    --enable-testing \
    --disable-obex \
    --disable-mesh \
    --disable-cups \
    --disable-manpages \
    --disable-systemd \
    --disable-monitor \
    --disable-udev >/dev/null

echo "==> building emulator/btvirt and tools/btmgmt"
make -j"$(nproc)" emulator/btvirt tools/btmgmt

echo
echo "btvirt built: $SRC/emulator/btvirt"
"$SRC/emulator/btvirt" --help 2>&1 | head -20 || true
echo "btmgmt built: $SRC/tools/btmgmt"
