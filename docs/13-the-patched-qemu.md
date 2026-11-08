# 13. The patched QEMU

## Short version

- The chip is emulated by **Espressif's fork of QEMU**, release `esp-develop-9.2.2-20260417` (QEMU 9.2.2), <https://github.com/espressif/qemu>.
- That fork does not emulate several ESP32-S3 peripherals that every board needs. We add them as new source files in [qemu/devices/](../qemu/devices/) and make a small number of **anchored edits** to Espressif's own files with [qemu/apply.sh](../qemu/apply.sh). We do not keep a fork: the fetched tree is Espressif's, untouched, until `apply.sh` runs.
- The result is `qemu-system-xtensa`. The emulator looks for it in `vendor/qemu/bin/`, or wherever `ESP32_EMULATOR_QEMU` points.
- QEMU is a separate program, started by the emulator as a child process. It is GPL-2.0 ([chapter 17](17-licences.md)).
- The stock binary Espressif publishes (`scripts/fetch-qemu.sh` downloads it) is **not enough** for the boards here: it has no S3 general-purpose SPI or I²C controller.

## What it is for

QEMU emulates the CPUs, RAM, flash, timers, UART and eFuse well. A board's screen, touch panel, keyboard and SD card hang off SPI and I²C, so those controllers have to exist and have to hand each transfer to something. The models in `qemu/devices/` are the controllers, and a socket client that forwards a transfer to the Rust side ([chapter 10](10-headless-and-trace.md), [architecture.md](architecture.md)).

## The files

| File | What it is |
|---|---|
| [qemu/devices/hw/ssi/esp32s3_gpspi.c](../qemu/devices/hw/ssi/esp32s3_gpspi.c) | The S3's general-purpose SPI (SPI2 and SPI3). Absent from the fork |
| [qemu/devices/hw/i2c/esp32s3_i2c.c](../qemu/devices/hw/i2c/esp32s3_i2c.c) | The S3's I²C0 and I²C1. Nothing instantiated one on the S3 |
| [qemu/devices/hw/misc/esp_vpb.c](../qemu/devices/hw/misc/esp_vpb.c) | The client side of the bus socket: sends each transfer to the emulator, waits for the reply |
| [qemu/devices/hw/misc/esp_gpio_spi.c](../qemu/devices/hw/misc/esp_gpio_spi.c), [esp_gpio_i2c.c](../qemu/devices/hw/misc/esp_gpio_i2c.c) | Decoders for firmware that drives SPI or I²C with plain GPIO writes (the boot menu reads the keyboard this way) |
| [qemu/devices/hw/misc/esp32s3_sens.c](../qemu/devices/hw/misc/esp32s3_sens.c) | The SAR ADC. Without it a battery-sense poll never completes and boot stops silently |
| [qemu/devices/hw/misc/esp32s3_ana_mst.c](../qemu/devices/hw/misc/esp32s3_ana_mst.c) | The analog master: the Wi-Fi PHY's readiness poll |
| [qemu/devices/hw/char/esp32s3_usb_serial_jtag.c](../qemu/devices/hw/char/esp32s3_usb_serial_jtag.c) | The native USB console |
| [qemu/apply.sh](../qemu/apply.sh) | Copies the new files in; edits Espressif's files at anchor lines; stops loudly if an anchor has gone |
| [qemu/build.sh](../qemu/build.sh) | Applies, configures, builds, copies the binary to `vendor/qemu/bin/` |
| [qemu/README.md](../qemu/README.md) | The long version, with the history of each model |
| [scripts/fetch-qemu-src.sh](../scripts/fetch-qemu-src.sh) | Clones Espressif's source at the tag into `vendor/src` |
| [scripts/fetch-qemu.sh](../scripts/fetch-qemu.sh) | Downloads Espressif's stock binary for this computer |
| [scripts/build-macos-deps.sh](../scripts/build-macos-deps.sh) | Builds the libraries QEMU needs as static archives ([chapter 14](14-building-on-macos.md)) |
| [scripts/run-emu.sh](../scripts/run-emu.sh), [sample-pc.sh](../scripts/sample-pc.sh) | Booting a flash image with QEMU directly; sampling where a hung boot is parked |

## How it works

### Why anchored edits, not a fork

`apply.sh` finds a line in Espressif's file by its text, not its number, and inserts after it. A version bump that moves lines still applies. If an anchor has disappeared it stops with the file named, instead of producing a tree that compiles into something subtly wrong. It can be run again safely: each edit checks whether it is already present.

### What the edits to Espressif's files do

Besides registering our models in the S3 machine (`hw/xtensa/esp32s3.c`) and the build files, `apply.sh` also:

- fixes three defects in Espressif's own models that stalled SD-card initialisation (a DMA channel lookup that matched unprogrammed channels, a wrong start-bit macro, and an interrupt matrix that ignored mapping changes). [qemu/README.md](../qemu/README.md), "Bugs fixed in the vendored tree";
- holds the second CPU in reset until the firmware releases it;
- makes `ssi-sd` selectable for Xtensa so an SD card can sit on the display's SPI bus;
- on Windows, lets the build copy where it cannot symlink, and gates the slirp block so `--disable-slirp` works ([chapter 16](16-building-on-windows.md));
- on macOS, stops meson using `strchrnul`, which exists only in the newest SDKs and would be a null call on older systems ([chapter 14](14-building-on-macos.md)).

### The command line

`crates/qemuctl` builds QEMU's arguments. Four settings are not optional (the PSRAM size and its octal mode, the strapping value, and the ROM directory); [qemu/README.md](../qemu/README.md), "Running a build", says what each one breaks. The emulator also passes `-global` settings that point every SPI and I²C controller at its bus server's TCP port, so a controller left out of that list silently reads every address as empty.

## How to build and test it

```sh
scripts/fetch-qemu-src.sh              # Espressif's source at the tag, into vendor/src
qemu/build.sh                          # apply our files, configure, build, install into vendor/qemu/bin/
ESP32_EMULATOR_REQUIRE_QEMU=1 cargo test -p qemuctl
```

The dependencies, per system, are in chapters [14](14-building-on-macos.md), [15](15-building-on-linux.md) and [16](16-building-on-windows.md). `libgcrypt` is needed on every system: the fork's `hw/misc/esp32_flash_enc.c` includes `gcrypt.h` unconditionally.

## What goes wrong

| Symptom | Cause |
|---|---|
| `apply.sh`: "anchor … matched N times, expected exactly 1" | The upstream file changed. Update the anchor; do not force it |
| Build stops: `'gcrypt.h' file not found` | `libgcrypt` is not installed, or was disabled |
| Boot stops after `phy_init: falling back to full calibration` | The analog master is not mapped. This is the stock binary |
| Every device reads as absent | A controller type was not given the bus server's port |
| `qemu-system-xtensa` not found | Not in `vendor/qemu/bin/`; set `ESP32_EMULATOR_QEMU` |

## What is not finished

- Only the **xtensa** target (ESP32 and ESP32-S3) is built. The RISC-V target for the ESP32-C3 is fetched by `fetch-qemu.sh riscv32` but none of our models are tied to it.
- No Wi-Fi, Bluetooth or LoRa radio ([architecture.md](architecture.md), section 6).
- The I²C and SPI controllers model what the firmware tested so far uses. Non-FIFO I²C mode is logged as not modelled.
- Untested: building the patched QEMU on Linux, on Apple Silicon, or on Windows today.
- The version is pinned to one Espressif tag. Moving to another means re-running `apply.sh` and fixing any anchor that moved.
