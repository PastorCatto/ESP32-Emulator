# ESP32-Emulator

An emulator for ESP32-S3 boards, built to run PURR OS and other ESP32 firmware
without the hardware. It opens a window with the board's screen, touch panel,
keyboard and SD card, or runs with no window so a script can press the screen,
take pictures and read the serial log.

It is made of two parts: a patched copy of Espressif's QEMU, which emulates the
chip, and a Rust program in this repository, which supplies everything the chip
talks to (display, touch, keyboard, SD card, Ethernet) and the window.

> **Status.** The ESP32-S3 boards boot real PURR OS images, with screen, touch,
> keyboard, SD card and (on the VM board) wired Ethernet. There is no Wi-Fi,
> Bluetooth or LoRa radio. The plain-ESP32 boards (the CYD family) boot to a
> serial console, but their screens are not modelled. Every chapter in
> [docs/](docs/README.md) ends with what is untested. The short list is at
> [the end of this page](#what-is-untested).

## What it needs

| | |
|---|---|
| Rust | 1.82 or newer is declared in `Cargo.toml`. Built here with 1.99.1; 1.82 itself has not been tried |
| The patched QEMU | `vendor/qemu/bin/qemu-system-xtensa`. Built from Espressif's fork plus the device models in [qemu/](qemu/). Stock QEMU does not work for these boards: it has no S3 SPI or I²C controller |
| A flash image | A 16 MB whole-flash image of the firmware (bootloader at 0x0). This repository does not ship one |
| A computer | Developed and tested on a Mac with an Intel processor. Apple Silicon, Linux and Windows are **untested**, see [macOS](docs/14-building-on-macos.md), [Linux](docs/15-building-on-linux.md), [Windows](docs/16-building-on-windows.md) |

## From a clone to a running window (macOS, Intel)

```sh
git clone https://github.com/PastorCatto/ESP32-Emulator.git
cd ESP32-Emulator

# 1. The patched QEMU. This is the slow part (tens of minutes; not timed).
scripts/fetch-qemu-src.sh                   # Espressif's source at the tag we patch
scripts/build-macos-deps.sh                 # glib, pixman, ... as static libraries, built for macOS 12
QEMU_DEPS_PREFIX=vendor/deps-macos12 qemu/build.sh

# 2. The program.
cargo build --release -p shell

# 3. Run it on a flash image and a board file.
target/release/esp32-emulator my-flash.bin boards/t-deck-plus-vm.toml --run
```

Step 1 needs `meson`, `ninja`, `pkg-config` and Python 3 on the path, and a C
compiler (the Xcode command line tools). Details, and what goes wrong, are in
[docs/14-building-on-macos.md](docs/14-building-on-macos.md).

**Not tested as one run.** Each step has been run on its own in the working
folder. The sequence above has not been run from a fresh `git clone`, because the
build files it needs were not yet committed when this page was written.

The emulator works on a copy of the flash image (in its run folder), so the file
you give it is never changed.

Without a window:

```sh
BOARD_TOML=boards/t-deck-plus-vm.toml HEADLESS_SHOTS=60 \
  cargo run --release -p shell --example headless_tap -- my-flash.bin 90 out.png
```

That boots for 90 seconds and writes `out.png`, plus `out-60.png` at second 60.
More in [docs/10-headless-and-trace.md](docs/10-headless-and-trace.md).

## The boards

Board files are data: `boards/*.toml` says which chip, which buses, which devices
sit at which pins. Adding a board is a file, not a patch
([docs/board-files.md](docs/board-files.md)).

| File | Board | Chip | Notes |
|---|---|---|---|
| `t-deck-plus-vm.toml` | LilyGO T-Deck Plus, VM flavour | ESP32-S3 | Wired Ethernet (W5500 model) instead of radios. The board PURR OS's VM image runs on |
| `t-deck-plus.toml` | LilyGO T-Deck Plus | ESP32-S3 | Screen, touch, keyboard, SD. Radio parts are declared but not modelled |
| `playtime.toml` | Playtime | ESP32-S3 | Display, SD card, motion sensor, and an input slot (game pad or keyboard, swappable while running). The display's chip-select line and rotation are guesses, marked in the file |
| `cyd-*.toml` (three) | Sunton CYD family | ESP32 | Boot to the serial console. Screens not modelled |
| `generic-esp32s3.toml` | Fallback | ESP32-S3 | |

## How it works

QEMU emulates the chip. Everything on the chip's buses lives in this repository,
on the other side of a socket:

```text
   ┌──────────────────────────┐        ┌────────────────────────────────┐
   │ QEMU (Espressif fork)    │        │ Emulator shell (Rust + egui)   │
   │                          │        │                                │
   │  Xtensa CPUs, RAM, flash │        │  ┌──────────────────────────┐  │
   │  UART, timers, eFuse     │        │  │ Registry: route by claim │  │
   │                          │        │  └────────────┬─────────────┘  │
   │  ┌────────────────────┐  │  vpb   │      ┌────────┴─────────┐      │
   │  │ SPI/I²C controller │──┼────────┼─────▶│ ST7789   GT911   │      │
   │  │ (we add these)     │  │ socket │      │ SD card  keyboard│      │
   │  └────────────────────┘  │        │      │ W5500    your    │      │
   └──────────────────────────┘        │      │ driver ──────────┼──┐   │
                                       │      └──────────────────┘  │   │
                                       └────────────────────────────┼───┘
                                                                    │
                                              any language, any process
```

A device is whatever answers for a bus address, so a driver can live in this
repository or in your own process ([docs/custom-hardware.md](docs/custom-hardware.md)).
Every transaction passes one place, so a bus tracer sees all of it
([docs/10-headless-and-trace.md](docs/10-headless-and-trace.md)).

## Documentation

| Chapter | |
|---|---|
| [docs/README.md](docs/README.md) | The index, with what each chapter covers |
| [docs/architecture.md](docs/architecture.md) | How it fits together and why each decision was made |
| [docs/board-files.md](docs/board-files.md) | The board TOML format, field by field |
| [docs/custom-hardware.md](docs/custom-hardware.md) | Three ways to add a device, including a working driver in Python |
| [docs/10-headless-and-trace.md](docs/10-headless-and-trace.md) | Running without a window; the command file; the bus trace |
| [docs/11-vm-image-layout.md](docs/11-vm-image-layout.md) | What a VM flash image must look like (flash map v2) |
| [docs/12-purr-run-and-the-sdk-kit.md](docs/12-purr-run-and-the-sdk-kit.md) | How the PURR SDK uses the emulator |
| [docs/13-the-patched-qemu.md](docs/13-the-patched-qemu.md) | What we add to QEMU and why |
| [docs/17-licences.md](docs/17-licences.md) | Licences, item by item |

## Layout of the repository

```text
crates/flashimg   Firmware image formats: chip IDs, app descriptors, partition
                  tables, flash assembly, empty LittleFS volumes.
crates/vpb        The bus contract: claims, transactions, routing, tracing, and
                  the wire protocol for drivers in other processes.
crates/boards     Board files: parsing and checking.
crates/devices    The device models: ST7789, GT911, keyboard, SD card, W5500,
                  VM info, MCP23017 game pad, ICM-42670 motion sensor.
crates/qemuctl    Finding, starting and controlling QEMU.
crates/shell      The window (esp32-emulator) and the headless runners
                  (examples/headless, examples/headless_tap).
boards/           Board definitions.
qemu/             Our additions to QEMU, and the scripts that apply and build them.
scripts/          Fetching and building QEMU and its libraries; helpers for runs.
docs/             The chapters.
vendor/           Fetched and built QEMU and libraries (not committed).
```

## What is untested

- A fresh `git clone` followed by the steps above.
- Any computer except an Intel Mac: Apple Silicon (under Rosetta 2 or native),
  Linux, Windows.
- Rust 1.82 (the declared minimum).
- `qemu/build.sh` with `QEMU_DEPS_PREFIX` run end to end; the static QEMU was
  built by hand with the same steps.
- Running the macOS 12 build on a Mac that is not running macOS 26: the file's
  recorded minimum is 12.0, but only macOS 26 has been available.
- The Windows build of the patched QEMU: it does not exist as a published
  binary, see [docs/16-building-on-windows.md](docs/16-building-on-windows.md).

## Licence and notices

The licence is per item; [LICENSES.md](LICENSES.md) is the table and
[docs/17-licences.md](docs/17-licences.md) says what was read and what was not.

- The window program (`crates/shell`) and the QEMU device models (`qemu/devices/`) are
  **GPL-2.0-or-later**, following QEMU: [LICENSE](LICENSE).
- The crates `flashimg`, `vpb`, `boards`, `devices` and `qemuctl` are **MIT OR Apache-2.0**:
  [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE).
- A built QEMU (not in this repository) is Espressif's QEMU fork plus our changes, and on macOS has
  glib, libintl, libgcrypt, libgpg-error, pcre2, libffi, pixman and libslirp linked in statically.
  Their licence texts and notices are in [third-party/](third-party/); the offer of source and the relink
  steps are in [SOURCE.txt](SOURCE.txt).

**NOTICE: Espressif ROM images.** QEMU for the ESP32-S3 loads Espressif's boot ROM
(`esp32s3_rev0_rom.bin`, from the `pc-bios` folder of https://github.com/espressif/qemu at tag
`esp-develop-9.2.2-20260417`). The ROM is Copyright (c) 2015-2022 Espressif Systems (Shanghai) Co. Ltd.
and is not open source. Espressif licenses the ROM *ELF* files under Apache-2.0; **the licence of the
`.bin` file itself is not stated anywhere that was found.** The ROM contains third-party code (Newlib, Xtensa libhal, miniz, Zephyr USB and others), listed in the notice with links to all the Espressif pages. This repository holds no ROM image. Details:
[third-party/espressif-rom/NOTICE.md](third-party/espressif-rom/NOTICE.md).
