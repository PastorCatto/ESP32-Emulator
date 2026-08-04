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

**Model.** Conversions complete immediately: writing the start bit sets the
done bit and loads a sample. That is not how the hardware behaves — a real
conversion takes microseconds — but nothing in firmware can observe the
difference through this interface, so modelling the delay would add a timer
and a state machine to buy nothing.

Clearing start also clears done, because a driver that re-arms by writing zero
would otherwise see a stale completion and read the previous sample.

The reported counts are properties (`adc1-raw`, `adc2-raw`, default 2048) so a
board can present a plausible battery level.

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

**2. Do not reconfigure incrementally after installing a package.** Meson
caches how it resolved each dependency. Installing `libgcrypt` mid-build and
letting ninja re-run configure produced a link line with *both*
`libglib-2.0.a` and `libglib-2.0.dll.a`, and hundreds of duplicate-symbol
errors. Use `--clean`.

**3. A build directory can be locked.** If a shell still has it open,
`rm -rf build` fails with "Device or resource busy", and the next configure
fails confusingly. `build.sh --clean` moves the directory aside instead of
insisting on deleting it.

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
