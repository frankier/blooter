#!/usr/bin/env bash
# Build btvirt from bluez source. No root required -- nothing is installed, we
# just want the emulator binary, which Fedora's bluez package does not ship
# (Debian has it in bluez-test-tools; Fedora has no equivalent).
set -euo pipefail

VERSION="${BLUEZ_VERSION:-5.87}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/build"
SRC="$BUILD/bluez-$VERSION"
TARBALL="bluez-$VERSION.tar.xz"
URL="https://mirrors.edge.kernel.org/pub/linux/bluetooth/$TARBALL"

if [[ -x "$SRC/emulator/btvirt" ]]; then
    echo "btvirt already built: $SRC/emulator/btvirt"
    exit 0
fi

# btvirt links against glib and dbus even though we disable most of bluez.
missing=()
for dep in glib-2.0 dbus-1; do
    pkg-config --exists "$dep" || missing+=("$dep")
done
if ((${#missing[@]})); then
    echo "Missing build dependencies: ${missing[*]}" >&2
    echo "On Fedora:  sudo dnf install glib2-devel dbus-devel" >&2
    exit 1
fi

mkdir -p "$BUILD"
cd "$BUILD"

if [[ ! -f "$TARBALL" ]]; then
    echo "==> fetching $URL"
    curl -fLO "$URL"
fi

if [[ ! -d "$SRC" ]]; then
    echo "==> extracting"
    tar xf "$TARBALL"
fi

cd "$SRC"
if [[ ! -f config.status ]]; then
    echo "==> configuring (--enable-testing is what builds emulator/btvirt)"
    # Everything not needed for the emulator is disabled: this keeps the
    # dependency surface small and the build to well under a minute.
    ./configure \
        --enable-testing \
        --disable-obex \
        --disable-client \
        --disable-mesh \
        --disable-cups \
        --disable-manpages \
        --disable-systemd \
        --disable-monitor \
        --disable-udev >/dev/null
fi

echo "==> building emulator/btvirt"
make -j"$(nproc)" emulator/btvirt

echo
echo "btvirt built: $SRC/emulator/btvirt"
"$SRC/emulator/btvirt" --help 2>&1 | head -20 || true
