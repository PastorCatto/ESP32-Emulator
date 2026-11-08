#!/usr/bin/env bash
# Build QEMU's library dependencies as static archives for a low macOS floor.
#
# Usage: scripts/build-macos-deps.sh [prefix]     (default vendor/deps-macos12)
#
# MacPorts and Homebrew build their libraries for the version of macOS they are
# installed on, so a QEMU linked against them refuses to start anywhere older
# (see qemu/README.md, "The macOS floor"). This builds the libraries QEMU
# needs from source into a private prefix, with the deployment target set and
# no shared libraries at all, so nothing has to ship beside the binary.
#
# What is left out on purpose: nettle, gnutls and zstd. The Espressif fork
# includes <gcrypt.h> directly (esp32_flash_enc.c), so libgcrypt is built. glib insists on a libintl even with its own translations
# off, so the runtime half of gettext is built too. zlib, bzip2 and iconv come from
# the macOS SDK. Nothing is installed outside the prefix.
#
# Then build QEMU against it:
#   QEMU_DEPS_PREFIX=vendor/deps-macos12 qemu/build.sh
#
# Tools needed on PATH: a C compiler, make, meson, ninja, pkg-config.
# Source tarballs are taken from $DEPS_CACHE (default vendor/deps-cache) and
# downloaded there when missing.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PREFIX="$(cd "$ROOT" && mkdir -p "${1:-vendor/deps-macos12}" && cd "${1:-vendor/deps-macos12}" && pwd)"
CACHE="${DEPS_CACHE:-$ROOT/vendor/deps-cache}"
WORK="${DEPS_WORK:-$ROOT/vendor/deps-work}"
JOBS="${JOBS:-4}"

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}"
# Only our own prefix: a stray .pc file from MacPorts or Homebrew would pull a
# library built for a newer macOS back in.
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_PATH=
export CFLAGS="-O2 -fPIC -mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
export CPPFLAGS="-I$PREFIX/include"
export LDFLAGS="-L$PREFIX/lib"

mkdir -p "$CACHE" "$WORK"

fetch() { # <file> <url>
  [ -f "$CACHE/$1" ] || { echo "Downloading $1"; curl -fsSL -o "$CACHE/$1" "$2"; }
}

unpack() { # <file> <dir>   -- always a fresh directory
  rm -rf "$WORK/$2"
  mkdir -p "$WORK/$2"
  tar -xf "$CACHE/$1" -C "$WORK/$2" --strip-components=1
}

fetch gettext-1.0.tar.xz    https://ftp.gnu.org/pub/gnu/gettext/gettext-1.0.tar.xz
fetch libgpg-error-1.61.tar.bz2 https://gnupg.org/ftp/gcrypt/libgpg-error/libgpg-error-1.61.tar.bz2
fetch libgcrypt-1.12.4.tar.bz2  https://gnupg.org/ftp/gcrypt/libgcrypt/libgcrypt-1.12.4.tar.bz2
fetch libffi-3.4.8.tar.gz   https://github.com/libffi/libffi/releases/download/v3.4.8/libffi-3.4.8.tar.gz
fetch pcre2-10.48.tar.bz2   https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.48/pcre2-10.48.tar.bz2
fetch pixman-0.46.0.tar.xz  https://cairographics.org/releases/pixman-0.46.0.tar.xz
fetch glib-2.88.3.tar.xz    https://download.gnome.org/sources/glib/2.88/glib-2.88.3.tar.xz
fetch libslirp-4.9.1.tar.bz2 https://gitlab.freedesktop.org/slirp/libslirp/-/archive/v4.9.1/libslirp-v4.9.1.tar.bz2

echo "== libffi"
unpack libffi-3.4.8.tar.gz libffi
(cd "$WORK/libffi" && ./configure --prefix="$PREFIX" --disable-shared --enable-static \
   --disable-docs >/dev/null && make -j"$JOBS" >/dev/null && make install >/dev/null)

echo "== pcre2"
unpack pcre2-10.48.tar.bz2 pcre2
(cd "$WORK/pcre2" && ./configure --prefix="$PREFIX" --disable-shared --enable-static \
   --enable-unicode --disable-pcre2-16 --disable-pcre2-32 --disable-pcre2grep-libz \
   --disable-pcre2grep-libbz2 >/dev/null && make -j"$JOBS" >/dev/null && make install >/dev/null)

echo "== pixman"
unpack pixman-0.46.0.tar.xz pixman
(cd "$WORK/pixman" && meson setup build --prefix="$PREFIX" --libdir=lib \
   --default-library=static -Dtests=disabled -Ddemos=disabled -Dgtk=disabled \
   -Dlibpng=disabled >/dev/null && ninja -C build -j"$JOBS" >/dev/null && ninja -C build install >/dev/null)

echo "== libintl"
unpack gettext-1.0.tar.xz gettext
(cd "$WORK/gettext/gettext-runtime" && ./configure --prefix="$PREFIX" --disable-shared \
   --enable-static --disable-libasprintf --disable-java --disable-csharp --disable-openmp \
   --without-emacs --with-included-gettext >/dev/null \
   && make -C intl -j"$JOBS" >/dev/null && make -C intl install >/dev/null)

echo "== glib"
unpack glib-2.88.3.tar.xz glib
(cd "$WORK/glib" && meson setup build --prefix="$PREFIX" --libdir=lib --wrap-mode=nofallback \
   --default-library=static -Dnls=disabled -Dtests=false -Dintrospection=disabled \
   -Dman-pages=disabled -Ddocumentation=false -Dlibmount=disabled -Dselinux=disabled \
   -Dxattr=false -Dsysprof=disabled -Dglib_debug=disabled -Dlibelf=disabled >/dev/null \
   && ninja -C build -j"$JOBS" >/dev/null && ninja -C build install >/dev/null)

echo "== libgpg-error"
unpack libgpg-error-1.61.tar.bz2 libgpg-error
(cd "$WORK/libgpg-error" && ./configure --prefix="$PREFIX" --disable-shared --enable-static \
   --disable-nls --disable-doc --disable-tests --disable-languages >/dev/null \
   && make -j"$JOBS" >/dev/null && make install >/dev/null)

echo "== libgcrypt"
unpack libgcrypt-1.12.4.tar.bz2 libgcrypt
(cd "$WORK/libgcrypt" && ./configure --prefix="$PREFIX" --disable-shared --enable-static \
   --disable-doc --with-libgpg-error-prefix="$PREFIX" >/dev/null \
   && make -j"$JOBS" >/dev/null && make install >/dev/null)

echo "== libslirp"
unpack libslirp-4.9.1.tar.bz2 libslirp
(cd "$WORK/libslirp" && meson setup build --prefix="$PREFIX" --libdir=lib --wrap-mode=nofallback \
   --default-library=static >/dev/null && ninja -C build -j"$JOBS" >/dev/null \
   && ninja -C build install >/dev/null)

# QEMU asks pkg-config for the shared-library form, which leaves out Libs.private,
# and a static archive cannot say what it needs without it. Fold the private
# libraries into Libs so the link line is complete.
for pc in "$PREFIX"/lib/pkgconfig/*.pc; do
  python3 - "$pc" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path).read()
m = re.search(r"^Libs\.private:(.*)$", text, re.M)
if m:
    text = re.sub(r"^Libs:(.*)$", lambda l: "Libs:" + l.group(1) + m.group(1), text, count=1, flags=re.M)
    open(path, "w").write(text)
if path.endswith("/libgcrypt.pc"):
    # Requires.private names libgpg-error, which Libs does not carry.
    text = re.sub(r"^Libs:(.*)$", lambda l: "Libs:" + l.group(1) + " -lgpg-error", text, count=1, flags=re.M)
    open(path, "w").write(text)
PY
done

echo
echo "Installed into $PREFIX. Check the archives' floor with:"
echo "  for f in $PREFIX/lib/*.a; do echo \$f; otool -l \$f | grep -m1 minos; done"
