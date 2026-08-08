# The QEMU side

Espressif's QEMU fork emulates the ESP32-S3 CPU, memory, flash and PSRAM well.
It does not emulate several peripherals real boards depend on. This directory
holds the device models we add, and the scripts that graft them into a
vendored source tree.

Nothing here is a fork. We keep our files separate and apply them to an
unmodified release tarball, so bumping QEMU is a re-download rather than a
merge.

## Layout

```text
devices/     new files, copied into the tree verbatim
  hw/misc/esp32s3_sens.c
  include/hw/misc/esp32s3_sens.h
apply.sh     copy devices in, then make anchored edits to QEMU's own files
build.sh     apply, configure, build, and install into vendor/qemu/bin
```

`apply.sh` edits QEMU's files by searching for an anchor line rather than by
line number, so a version bump that shifts lines still applies. If an anchor
genuinely disappears it stops with an error naming the file, instead of
silently producing a tree that compiles into something subtly wrong. It is
safe to run repeatedly.

## What we add, and why

### `esp32s3_sens` — the SAR ADC

**Symptom.** Real T-Deck firmware boots normally, prints its startup banner,
and then goes silent forever. Both cores sit at one address.

**Cause.** Nothing is mapped at `DR_REG_SENS_BASE` (`0x60008800`) for the S3.
Firmware starts an ADC conversion and polls for completion:

```asm
l32i.n a9, a8, 12          ; read [a8 + 0x0c], with a8 = 0x60008800
bbci   a9, 16, <loop>      ; spin while bit 16 is clear
```

`SENS_BASE + 0x0c` is `SENS_SAR_MEAS1_CTRL2_REG` and bit 16 is
`SENS_MEAS1_DONE_SAR`. The done bit never sets, so the poll never ends. On a
T-Deck this is the battery sense on GPIO 4.

**Model**, now measured rather than inferred:

Conversions complete immediately — confirmed on hardware, where the done bit
and the sample are both valid by the CPU's first read after starting one.

**Clearing start does *not* clear done**, and the sample goes stale rather
than being invalidated:

```
w 6000880c 0x60000  ->  readback 0x000709ec   (start+force, done, sample)
w 6000880c 0x0      ->  readback 0x000109ec   (done still set, sample stale)
```

DATA and DONE are hardware-owned: guest writes do not reach them and they
survive until the next conversion.

This corrects an earlier version that cleared done on that write, on the
reasoning that a driver re-arming by writing zero should not see a stale
completion. That reasoning was sound and the hardware does not honour it —
which made the emulator **safer than the silicon**. That is the worst
direction for a divergence to point, because firmware carrying the race
passes here and is flaky on the board. Prefer a faithful model to a kind one.

The reported counts are properties (`adc1-raw`, `adc2-raw`). The default 2528
is a live T-Deck battery reading, not a mid-scale placeholder.

### `esp32s3_gpspi` — general-purpose SPI

GP-SPI2 and GP-SPI3, the controllers every non-flash SPI device hangs off. The
fork models `SPI_MEM` (flash and PSRAM) and instantiates it as `spi1`, but
GP-SPI is a different peripheral with a different register map and no model
existed for any chip.

Several behaviours here are **measured on a real T-Deck Plus** via the PURR OS
hardware probe rather than inferred from the TRM. Each is worth knowing
because each independently hangs firmware:

- **The clock gate.** `SPI_CLK_GATE` must be non-zero or the entire register
  file reads as zero — `SPI_DATE` included, and that is a hardwired constant.
  IDF sets it when a *device* is added to the bus, not when the bus is
  initialised. Firmware correct in every other respect hangs on this.
- **16× mirroring.** The file is `0x100` bytes repeated across a 4 KiB window.
  Decoding the full 12 bits returns zero where hardware returns live values.
- **`SPI_DMA_CONF` does not read back what you write.** Bits 0–1 re-assert:
  write `0x00000000`, read `0x00000003`.
- **MISO reads `0x00`** on this board, not `0xFF`. The ST7789 shares MISO with
  SD and LoRa and does not drive it. Reasoning from "floating lines read high"
  gives the wrong answer here, which is the argument for measuring.
- **Completion is reported on a timer**, not inside the write that starts the
  transfer, so the ISR cannot re-enter the driver before it finishes its
  post-start bookkeeping.
- **The interrupt line lags `ENA` being masked.** Hardware enters the handler
  exactly twice; a synchronous deassert gives one entry and diverges silently.

### `esp32s3_usb_serial_jtag` — the native USB console

The fork maps this peripheral but implements it as a stub: reads return zero,
writes are dropped, the state struct has one field. Firmware built with
`CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` therefore boots **completely silently** —
which covers any board whose USB-C goes straight to the S3 rather than through
a bridge chip.

Modelled against the ESP-IDF HAL (`usb_serial_jtag_ll.h`) rather than the
register reference, since the HAL is the definitive statement of what firmware
expects. The 64-byte endpoint auto-flushes when full as well as on `WR_DONE`,
because a driver may write a long string and flush only at the end.

UART0 and UART1 hold `serial_hd(0)` and `(1)`, so this takes the third slot:

```sh
qemu-system-xtensa ... -serial null -serial null -serial stdio
```

## Building

### Dependencies

On Windows, from an **MSYS2 MINGW64** shell:

```sh
pacman -S --needed \
  mingw-w64-x86_64-gcc \
  mingw-w64-x86_64-glib2 \
  mingw-w64-x86_64-pixman \
  mingw-w64-x86_64-ninja \
  mingw-w64-x86_64-meson \
  mingw-w64-x86_64-python \
  mingw-w64-x86_64-pkgconf \
  mingw-w64-x86_64-zlib \
  mingw-w64-x86_64-libslirp \
  mingw-w64-x86_64-libgcrypt \
  git make diffutils
```

`libgcrypt` is easy to miss: stock QEMU does not need it, but this fork's
`hw/misc/esp32_flash_enc.c` includes `gcrypt.h` unconditionally.

On Linux and macOS the same packages come from the system package manager, and
none of the Windows-specific notes below apply.

### Running it

```sh
qemu/build.sh            # incremental
qemu/build.sh --clean    # after changing installed packages
```

The result is copied to `vendor/qemu/bin/`, which is where `Qemu::locate`
looks first — so the emulator picks up your build without any extra
configuration.

## Three Windows gotchas

These each cost real time, so they are written down.

**1. Symlinks abort configure.** QEMU's `scripts/symlink-install-tree.py` uses
`os.symlink`, which Windows refuses without Developer Mode or elevation. The
failure aborts configure entirely, long after everything useful has been
generated. `apply.sh` patches the script to fall back to copying.

The fallback is best-effort on purpose. Some entries are build artifacts that
do not exist yet at configure time — a symlink can point at a path that will
appear later, a copy cannot — and the bundle tree is only a convenience for
running from the build directory. So a missing entry is skipped rather than
raised. Copying and *then* failing on a missing source is a trap worth
avoiding: it looks like the fix worked right up until it doesn't.

**2. slirp drags in a second, static glib — and `--disable-slirp` does not
work.** This presents as hundreds of `multiple definition of
'g_main_context_ref'` errors at link time, with both `libglib-2.0.a` and
`libglib-2.0.dll.a` on the command line.

There are two separate bugs in this fork's `meson.build`, and you have to fix
the second to escape the first.

The dependency is requested with `static: true` hardcoded:

```meson
slirp_dep = dependency('slirp', required: get_option('slirp'),
                       method: 'pkg-config',
                       static: true)
```

MSYS2 ships libslirp as a static library only, so meson resolves *its* glib
dependency with `pkg-config --static` and returns the static archive — while
QEMU has already found glib as a DLL import library. Both get linked.

The obvious escape is `--disable-slirp`, but the block calls
`declare_dependency()` unconditionally, with no `if slirp_dep.found()` guard.
So `slirp` stays truthy even when the dependency was skipped,
`net/slirp.c` is still compiled, and it fails on a missing `libslirp.h`.

`apply.sh` gates the whole block on `.allowed()`, which fixes both: disabled
means the block is skipped and `slirp` keeps its `not_found` value.

Reaching for a clean rebuild first is tempting and does not help — the cause
is dependency resolution, not stale state. (A clean rebuild *is* needed after
changing installed packages, but that is a different problem.)

**3. A build directory can be locked.** If a shell still has it open,
`rm -rf build` fails with "Device or resource busy", and the next configure
fails confusingly. `build.sh --clean` moves the directory aside instead of
insisting on deleting it.

## Running a build

```sh
scripts/run-emu.sh <flash.bin> [seconds] [sd.img]
```

It starts the vpb peripheral server when given an image, wires the SPI
controllers to it, and leaves `serial.log`, `vpb.log` and `qemu.log` in a
temporary directory whose path it prints. `QEMU_DEBUG=unimp,guest_errors`
passes through to `-d`, which is how the device models' own traces surface.

Four settings are not optional and each one fails in its own way:

| Setting | Why |
| --- | --- |
| `-m 8M` | the machine turns `-m` into the size of the PSRAM chip on spi1, not host memory |
| `-global ssi_psram.is_octal=true` | the S3 machine never sets it, so it defaults to quad; every `-R8` module is octal and IDF's octal driver rejects a quad chip |
| `strap_mode=0x04` | `ESP32S3_STRAP_MODE_FLASH_BOOT`. The ESP32 value (`0x12`) and the S3 UART value both land in download mode |
| `-L vendor/qemu/share/qemu` | where `esp32s3_rev0_rom.bin` lives; without it the ROM is "not found" |

`scripts/sample-pc.sh` samples both cores' registers over the QEMU monitor
after a settle delay, for finding where a hung boot is parked. `MON_EXTRA`
runs extra monitor commands once — reading interrupt matrix mappings with
`xp /1wx` is what identified the bug below.

## Bugs fixed in the vendored tree

Three defects in Espressif's own models, all patched through `apply.sh` so
they survive re-vendoring. They shared a symptom — SD card init hanging, which
in turn held the SPI bus lock and stopped the display from ever initialising.

**1. `esp_gdma_get_channel_periph` matched channels nobody programmed.** The
lookup reads `peripheral == periph || started` where its own comment says the
channel "must be marked as 'started' too". Separately, channel state is
`memset` to zero on reset, but an unbound `PERI_SEL` reads `0x3F` on silicon,
not `0`. Zero is SPI2's trigger id, so every unbound channel claimed to be
bound to SPI2, and the OR handed one back before the real channel was
considered. The transfer then chased a descriptor at address 0 — a flood of
rejected reads at `0x0/0x4/0x8`, two per transfer, while reporting success.

**2. The same lookup read the wrong START bit.** `IN_LINK` and `OUT_LINK` do
not share a layout:

```
IN_LINK:   ADDR[19:0] AUTO_RET(20) STOP(21) START(22) RESTART(23) PARK(24)
OUT_LINK:  ADDR[19:0]              STOP(20) START(21) RESTART(22) PARK(23)
```

It used the `OUT_LINK` macro for both directions, so on an RX channel it
tested `INLINK_STOP`. Harmless while the condition was an OR; load-bearing the
moment it became an AND.

**3. The interrupt matrix ignored mapping changes.** It forwarded a source's
level to a CPU interrupt only when the *source* toggled. Hardware is
combinational in both inputs, and ESP-IDF depends on that: `esp_intr_disable`
does not mask the CPU interrupt, it rewrites the matrix entry to
`INT_MUX_DISABLED_INTNO` (6). From `spi_master.c`'s own header comment:

> If SPI is done transmitting/receiving but nothing is in the queue, it will
> not clear the SPI interrupt but just disable it by `esp_intr_disable`. This
> way, when a new thing is sent, pushing the packet into the send queue and
> re-enabling the interrupt (by `esp_intr_enable`) will trigger the interrupt
> again.

So the wakeup for a queued transaction is a *mapping* write against a line the
peripheral has held high since the previous transfer. Dropping it meant the
first `spi_device_transmit` after any polling traffic never started. This is
what `sdspi` does to read a CID or a data block, so SD init stopped dead after
CMD10 with the task blocked on its semaphore forever.

The symptom was unambiguous once measured: the peripheral held IRQ high,
`INTENABLE` had bit 9 set, the matrix mapped SPI2 to 9 — and the CPU's
`INTERRUPT` register never saw it.

The fix recomputes each CPU interrupt as the OR of every source mapped to it,
which is also what shared interrupts need.

## Adding another device model

1. Write `devices/hw/<subsystem>/<name>.c` and
   `devices/include/hw/<subsystem>/<name>.h`.
2. Add an `insert_after` call in `apply.sh` for the meson entry, and for each
   edit to `hw/xtensa/esp32s3.c`: the include, the SoC struct field, the
   `object_initialize_child`, and the realize plus map.
3. `qemu/build.sh`

Match the conventions of the surrounding tree rather than current upstream
QEMU. This fork is on 9.2.2 and still uses:

- `class_init(ObjectClass *klass, void *data)` — not `const void *`
- `static Property x[]` ending in `DEFINE_PROP_END_OF_LIST()` — not a
  `const` array without a terminator
- `ResettableClass::phases.hold` — not `device_class_set_legacy_reset`

Copying a newer upstream idiom compiles nowhere and the errors are unhelpful.
