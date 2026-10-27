# OS handoff

From the **emulator session** to the **PURR OS session** and the owner. Written 2026-10-26.

Things found on the emulator side that the OS side can act on, with the numbers behind them. This
file lives in the emulator repo. The PURR OS session is asked to keep a matching file in its repo for
the other direction, so each side can read the other's without editing it. The older shared file,
`~/Projects/PURR-OS-ESP32/emulator notes.md`, is still there, but sections 3.1 and 3.4 of it are out of
date (the W5500 model was built in Rust, not inside QEMU).

Nothing here asks the OS to change what it verifies or trusts. Every number below was measured with the
signed `Testrun/vm-secure` image running unmodified.

## 1. What was slow, and why

The boot package draws the menu by driving the display's four wires with single GPIO writes (24 writes
per data byte). Every write is a trap into QEMU, about 1.3 µs each. That makes the bit-banged display
the slowest early step:

| Operation | Bytes | GPIO writes |
|---|---|---|
| Full-screen clear (`hw_fill(0,0,320,240,BLACK)` in `hw_display_init`) | 153,600 | ~3.7 million |
| Countdown footer strip, redrawn every second (`draw_footer`, 320x28) | 17,920 | ~430,000 per redraw |

Before the fix below, "display up" came at about 9.5 s of guest time on the secure image (the README says
7.4 s), and the menu handed off at about 21.8 s.

## 2. What the emulator did (done, measured)

A sample of QEMU while the display started showed both guest-CPU threads spending most of their time
waiting for QEMU's global lock, which every memory-mapped access takes. The second core was running the
ROM's wait loop from power-on, reading a register as fast as it could, and every one of those reads
competed with core 0's GPIO writes. Core 0 spent about a quarter of its samples inside lock waits.
Core 1's program counter sat at `0x40043a3e` in the ROM on every sample.

Cause: QEMU's S3 model ignored `SYSTEM_CORE_1_CONTROL_0` (the register ESP-IDF's `start_other_core()`
uses to release core 1) and its stall handler was an empty stub, so core 1 ran from power-on. On silicon
it is held in reset (`RESETING` set, clock gated) until the OS releases it. The emulator now models
that: core 1 starts stalled and is released when the register says run (clock on, no stall, not in
reset). A second bug had to be fixed along the way: stalling a core that has never run leaves a halt
request pending, which makes it halt again the moment it is released.

Result on `Testrun/vm-secure/flash.bin`, guest time:

| Event | Before | After |
|---|---|---|
| Boot menu starts | 2.2 s | 0.5 s |
| Display up | ~9.5 s | 2.1 s |
| Boot menu hands off | ~21.8 s | 8.3 s |
| `PURR OS starting` | ~22 s | 8.5 s |
| `vm: running in a VM` | ~34 s | 20.4 s |
| Ethernet link up, `eth ip: 10.0.2.15` | ~34 s | 21.6 s |

The dual-core OS still starts normally (`Pro cpu start user code` follows `Multicore app`). The change is
in `qemu/apply.sh` (section "The app CPU starts held in reset"), uncommitted on branch `vm/w5500`.

**Second pass.** A fresh sample showed core 0 was still mostly waiting for the global lock, this time against
QEMU's main loop. The cause was on the emulator's own display path: the vpb client re-armed its flush timer
on every transfer, and moving a timer that is already the earliest wakes the main loop each time. The bit-bang
decoder made one transfer per byte, so a screen was ~150,000 wakeups. Now the decoder batches bytes (flushing on a
data/command change, a chip-select edge, 4 KiB, or 2 ms), and the client arms its timer once per batch. The client
change also covers the SPI controller path that Arduino-style firmware uses (about 127,000 transfers a redraw),
though no Arduino image was available to check that here.

| Event (guest time) | Original | After pass 1 | After pass 2 |
|---|---|---|---|
| Display up | ~9.5 s | 2.1 s | 1.3 s |
| Boot menu hands off | ~21.8 s | 8.3 s | 5.7 s |
| `PURR OS starting` | ~22 s | 8.5 s | 5.9 s |
| `eth ip: 10.0.2.15` | ~34 s | 21.6 s | 19.1 s |

The old-layout DP8 image, which draws through the SPI controller, still draws correctly and is about a
second faster to the touch panel (6.7 s to 5.7 s). What is left between "PURR OS starting" and the lease (about
13 s) is the OS itself running on emulated CPU speed.

## 3. What the OS side can still do (owner's call, VM flavor only)

The remaining cost is the number of GPIO writes, which only the OS can reduce:

1. **Skip the full-screen black clear** in the VM flavor. The emulated panel starts black, so the ~3.7
   million writes in `hw_display_init` change nothing there. On hardware the clear is needed.
2. **Redraw only what changed in the countdown.** The footer clears a 320x28 strip every second to change
   one digit. Drawing only the digit's cell would cut ~430,000 writes per redraw to a few thousand.
3. The menu's wait for a key is now about 6 s of guest time rather than 14 s, probably because the redraws
   that counted against it are faster. Worth a look if the wait is meant to be a fixed duration.

I have not changed any of the OS code, and I would not: these are behaviours of the boot package.

## 4. Open on the emulator side

- **The shared-bus stall** in `plan4.md` is handled, so nothing is planned here. The fix on the other side
  was a flag: the VM-info device tells the OS it is a VM, and the bootloader and OS then address the
  Ethernet controller on its own bus (SPI3) instead of sharing SPI2 with the display. For the record, the
  cause in the SPI controller model (a lost completion interrupt when a polling device and queued display
  transfers interleave) was never explained, because the separate bus avoids it. If it ever shows up again
  on a shared bus, that is the place to look.
- Further speed-up options, riskier: a lock-free path for GPIO writes. Not started.
- HTTPS fetch, loader-slot recovery and the eFuse burn are untested on both sides.

## 5. How to reproduce the measurement

Run the image from a scratch copy, then sample QEMU while the display starts (macOS):
`sample <qemu pid> 6 -file out.txt`, and read the "Sort by top of stack" section for the two guest-CPU
threads. Ask each core for its program counter with QEMU's monitor: start with
`ESP32_QEMU_ARGS="-qmp tcp:127.0.0.1:4455,server,nowait"` and send `info registers -a`.

## 6. Status of the emulator repo

All of this work is uncommitted in the working tree of branch `vm/w5500`, mixed in with the PURR OS
session's W5500 changes. The owner decides how to commit it. My own seven commits on
`vm/new-boot-chain` are underneath and untouched.
