#!/usr/bin/env bash
#
# Sample the guest PC on both cores while the emulator runs, to find where a
# hung boot is actually parked.
#
# A stall shows up as every sample landing in the same routine. That alone
# does not name the caller, so resolve the addresses against the ELF with
# `cargo run -p flashimg --example addr2sym` afterwards -- and disassemble
# before believing a symbol, because the nearest preceding name is often not
# the function the address is in.
#
# Usage: scripts/sample-pc.sh <flash.bin> [settle_seconds] [samples] [sd.img]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
QEMU="$ROOT/vendor/qemu/bin/qemu-system-xtensa.exe"

FLASH="${1:?usage: sample-pc.sh <flash.bin> [settle] [samples] [sd.img]}"
SETTLE="${2:-40}"
SAMPLES="${3:-12}"
SD_IMG="${4:-}"
MON_PORT=55555
VPB_PORT=5559

win() { cygpath -w "$1"; }
CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo.exe")"

LOG="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null || true' EXIT

VPB_ARGS=()
if [ -n "$SD_IMG" ]; then
  "$CARGO" run --quiet -p vpb --example listen --features devices/sdcard \
    -- "$VPB_PORT" "$(win "$SD_IMG")" >"$LOG/vpb.log" 2>&1 &
  for _ in $(seq 50); do
    grep -q listening "$LOG/vpb.log" 2>/dev/null && break
    sleep 0.1
  done
  VPB_ARGS=(-global driver=ssi.esp32s3.gpspi,property=vpb-port,value=$VPB_PORT)
fi

"$QEMU" \
  -nographic -machine esp32s3 \
  -L "$(win "$ROOT/vendor/qemu/share/qemu")" \
  -m "${PSRAM_MB:-8}"M \
  -global driver=ssi_psram,property=is_octal,value=true \
  -drive file="$(win "$FLASH")",if=mtd,format=raw \
  -global driver=esp32s3.gpio,property=strap_mode,value=0x04 \
  "${VPB_ARGS[@]}" \
  -serial file:"$(win "$LOG/uart0.log")" \
  -serial file:"$(win "$LOG/uart1.log")" \
  -serial file:"$(win "$LOG/serial.log")" \
  -monitor tcp:127.0.0.1:$MON_PORT,server,nowait \
  >"$LOG/qemu.log" 2>&1 &
QEMU_PID=$!

sleep "$SETTLE"

# One monitor session, all samples: reconnecting per sample costs more wall
# time than the interval being sampled.
# Bash's /dev/tcp rather than python: this box only has the Windows Store
# python stub on PATH, which prints an advert and exits.
#
# The reader runs as a background job draining into the log, so the writer
# never blocks on a full socket buffer.
exec 3<>"/dev/tcp/127.0.0.1/$MON_PORT"
cat <&3 >"$LOG/mon.log" &
READER=$!
# MON_EXTRA is any additional monitor commands to run once, newline separated,
# e.g. MON_EXTRA=$'xp /1wx 0x600c2038' to read an interrupt matrix mapping.
if [ -n "${MON_EXTRA:-}" ]; then
  printf '%s\n' "$MON_EXTRA" >&3
  sleep 0.5
fi

for _ in $(seq "$SAMPLES"); do
  # Both cores: the blocked task and the idle one look nothing alike, and
  # only one of them is interesting.
  printf 'cpu 0\ninfo registers\ncpu 1\ninfo registers\n' >&3
  sleep 0.4
done
printf 'quit\n' >&3
sleep 0.5
kill "$READER" 2>/dev/null || true
exec 3<&- 3>&-

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

echo "=== PC samples ==="
grep -oiE "\bpc=0x[0-9a-f]+" "$LOG/mon.log" | sort | uniq -c | sort -rn | head
echo "=== interrupt state ==="
grep -oiE "\b(INTENABLE|INTERRUPT|INTSET|PS)=0x[0-9a-f]+" "$LOG/mon.log" \
  | sort | uniq -c | sort -rn | head -12
echo "logs: $LOG"
