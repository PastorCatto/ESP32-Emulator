#!/usr/bin/env bash
# Build the Espressif QEMU fork with our device models applied.
#
# Usage: qemu/build.sh [--clean]
#
# Expects the source in vendor/src (see scripts/fetch-qemu-src.sh).
# On Windows this must run from an MSYS2 MINGW64 shell; see qemu/README.md.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SRC="$ROOT/vendor/src"
BUILD="$SRC/build"

if [ ! -f "$SRC/hw/xtensa/esp32s3.c" ]; then
  echo "error: no QEMU source at $SRC" >&2
  echo "       run scripts/fetch-qemu-src.sh first" >&2
  exit 1
fi

if [ "${1:-}" = "--clean" ]; then
  # Windows can hold a lock on a directory that a shell has open, and the
  # failure surfaces much later as a confusing configure error. Move the old
  # tree aside rather than insisting on deleting it.
  if [ -d "$BUILD" ]; then
    echo "Moving aside the previous build directory"
    mv "$BUILD" "$BUILD.old.$$" 2>/dev/null || {
      echo "warning: could not move $BUILD; building into build.new" >&2
      BUILD="$SRC/build.new"
    }
    rm -rf "$SRC"/build.old.* 2>/dev/null || true
  fi
fi

bash "$HERE/apply.sh" "$SRC"

# Reconfiguring incrementally after installing a new dependency mixes two
# different dependency resolutions and produces duplicate-symbol link errors
# (glib resolved both shared and static). Configure only when there is no
# build.ninja, and use --clean after changing the installed packages.
if [ ! -f "$BUILD/build.ninja" ]; then
  echo "Configuring"
  mkdir -p "$BUILD"
  (cd "$BUILD" && ../configure \
      --target-list=xtensa-softmmu \
      --disable-werror \
      --disable-docs \
      --disable-gtk \
      --disable-sdl \
      --disable-vnc \
      --disable-curses \
      --disable-spice \
      --disable-opengl \
      --disable-capstone \
      --disable-slirp)
fi

echo "Building"
(cd "$BUILD" && ninja qemu-system-xtensa.exe 2>/dev/null || ninja qemu-system-xtensa)

# Publish next to the vendored release build so Qemu::locate finds it.
OUT="$ROOT/vendor/qemu/bin"
mkdir -p "$OUT"
for name in qemu-system-xtensa.exe qemu-system-xtensa; do
  if [ -f "$BUILD/$name" ]; then
    cp "$BUILD/$name" "$OUT/$name"
    echo "Installed $OUT/$name"
  fi
done

echo
echo "Verify with:"
echo "  ESP32_EMULATOR_REQUIRE_QEMU=1 cargo test -p qemuctl"
