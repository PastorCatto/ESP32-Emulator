#!/usr/bin/env bash
#
# Boot a flash image under the emulator, optionally with a vpb peripheral
# server attached.
#
# The invocation is fiddly enough -- three serial slots, a strapping override,
# a global property per SPI controller -- that reconstructing it by hand each
# time is how you end up debugging the command line instead of the emulator.
#
# Usage:
#   scripts/run-emu.sh <flash.bin> [seconds] [sd.img]
#
# Serial layout: UART0 and UART1 take the first two slots because the machine
# wires them unconditionally, so the USB Serial/JTAG console -- the one the
# probe and PURR OS actually talk on -- lands on the third.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
QEMU="$ROOT/vendor/qemu/bin/qemu-system-xtensa.exe"

FLASH="${1:?usage: run-emu.sh <flash.bin> [seconds] [sd.img]}"
SECONDS_TO_RUN="${2:-25}"
SD_IMG="${3:-}"
PORT=5559

# The machine turns -m into the size of the PSRAM chip it hangs off spi1, so
# this is not host memory. A T-Deck Plus carries 8 MB of octal PSRAM, and a
# build with external .bss aborts in cpu_start if it is missing.
#
# The S3 machine instantiates the PSRAM model without ever setting is_octal,
# so it defaults to quad. Every S3 module with 8 MB or more is octal (the
# -R8 parts), and IDF's octal driver rejects a quad chip outright with
# "PSRAM chip is not connected, or wrong PSRAM line mode". Override it here
# rather than in the machine, because -R2 parts really are quad.
PSRAM_MB="${PSRAM_MB:-8}"
PSRAM_OCTAL="${PSRAM_OCTAL:-true}"

[ -x "$QEMU" ] || { echo "no emulator at $QEMU -- run qemu/build.sh" >&2; exit 1; }

LOG="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null || true' EXIT

# The emulator is a native Windows binary and cannot open an MSYS path, so
# anything handed to it as a filename goes through cygpath first.
win() { cygpath -w "$1"; }

CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo.exe")"

VPB_ARGS=()
if [ -n "$SD_IMG" ]; then
  "$CARGO" run --quiet -p vpb --example listen --features devices/sdcard \
    -- "$PORT" "$(win "$SD_IMG")" >"$LOG/vpb.log" 2>&1 &
  # The emulator's connect is not retried, so the listener has to be up first.
  for _ in $(seq 50); do
    grep -q "listening" "$LOG/vpb.log" 2>/dev/null && break
    sleep 0.1
  done
  # The long form is required: the type name contains dots, so the
  # `-global type.prop=value` shorthand parses the wrong split point.
  VPB_ARGS=(-global driver=ssi.esp32s3.gpspi,property=vpb-port,value=$PORT)
fi

# QEMU_DEBUG is passed straight to -d, e.g. QEMU_DEBUG=unimp,guest_errors to
# see the device models' own traces. Output lands in qemu.log with the rest.
DEBUG_ARGS=()
[ -n "${QEMU_DEBUG:-}" ] && DEBUG_ARGS=(-d "$QEMU_DEBUG")

"$QEMU" \
  -nographic -machine esp32s3 \
  "${DEBUG_ARGS[@]}" \
  -L "$(win "$ROOT/vendor/qemu/share/qemu")" \
  -m "$PSRAM_MB"M \
  -global driver=ssi_psram,property=is_octal,value="$PSRAM_OCTAL" \
  -drive file="$(win "$FLASH")",if=mtd,format=raw \
  -global driver=esp32s3.gpio,property=strap_mode,value=0x04 \
  "${VPB_ARGS[@]}" \
  -serial null -serial null -serial file:"$(win "$LOG/serial.log")" \
  >"$LOG/qemu.log" 2>&1 &
QEMU_PID=$!

sleep "$SECONDS_TO_RUN"
kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

# The logs outlive the run so a failure can be picked apart afterwards
# instead of being reconstructed from a truncated terminal dump.
echo "logs: $LOG"
for f in serial vpb qemu; do
  [ -s "$LOG/$f.log" ] && echo "  $f.log  $(wc -l <"$LOG/$f.log") lines"
done
exit 0
