# ESP32-Emulator

A cross-platform emulator for ESP32-family boards. Drag a firmware image onto
the window and run it — serial console, display, SD card, and a simulated Wi-Fi
environment, without the hardware.

The first target is the LilyGO T-Deck Plus (ESP32-S3). Boards are data, so
supporting another one is a config file rather than a patch.

> **Status: early, but it boots.** Real LilyGO T-Deck Plus firmware runs
> through the ROM, the bootloader, PSRAM init, and into application code, with
> a working serial console. There is no display yet — see
> [Where this actually is](#where-this-actually-is).

## How it works

QEMU emulates the SoC. Everything hanging off the SoC's buses lives in this
repo instead, on the other side of a socket:

```text
   ┌──────────────────────────┐        ┌────────────────────────────────┐
   │ QEMU (Espressif fork)    │        │ Emulator shell (Rust + egui)   │
   │                          │        │                                │
   │  Xtensa CPU, RAM, flash  │        │  ┌──────────────────────────┐  │
   │  UART, timers, eFuse     │        │  │ Registry: route by claim │  │
   │                          │        │  └────────────┬─────────────┘  │
   │  ┌────────────────────┐  │  vpb   │      ┌────────┴─────────┐      │
   │  │ SPI/I²C controller │──┼────────┼─────▶│ ST7789   GT911   │      │
   │  │ (we add these)     │  │ socket │      │ SD card  keyboard│      │
   │  └────────────────────┘  │        │      │ your driver ─────┼──┐   │
   └──────────────────────────┘        │      └──────────────────┘  │   │
                                       └────────────────────────────┼───┘
                                                                    │
                                              any language, any process
```

Espressif's QEMU fork emulates the CPU, memory, and flash well, but **not** GP
SPI, I²C, SD/MMC, or Wi-Fi on the S3 — and every T-Deck peripheral hangs off
SPI or I²C. So we add the *bus controllers* to QEMU and implement the *devices*
here in Rust, where they can be iterated on without a C rebuild.

That split is also what makes custom hardware cheap: a device is whatever
answers for a bus address, whether it lives in this repo or in your own process.

## Modularity

Three levels, documented in [docs/custom-hardware.md](docs/custom-hardware.md):

1. **Board TOML** — panel size, pin assignments, I²C addresses. No rebuild.
2. **In-tree driver** — implement `Peripheral`, gate it behind a Cargo feature.
3. **External driver** — speak the wire protocol from any language. No rebuild,
   nothing of yours in this tree.

Every device is an independent switch: turn one off and the bus behaves as
though the chip were absent, which is how you find out whether a peripheral is
what firmware is choking on.

## Bus tracer

Off by default, per-bus toggles. Every transaction routes through one place, so
tracing sees all of it:

```text
I²C0 → 0x5d gt911     w[81 4e]
I²C0 ← 0x5d gt911     r[01 78 00 58 00]  · 1 touch point @ (120, 88)
I²C0 ← 0x77 <unclaimed>
SPI2 → cs12 st7789    w[2a 00 00 01 3f]  · CASET x=0..319
```

Drivers decode their own traffic, so you can confirm the *right* commands went
out, not just that bytes moved. Large transfers are truncated rather than
flooding the log.

## Repository layout

```text
crates/flashimg   firmware image formats: chip IDs, app descriptor,
                  partition tables, flash assembly. Zero dependencies.
crates/vpb        the bus contract: claims, transactions, routing,
                  tracing, and the external-driver wire protocol.
boards/           board definitions as TOML.
docs/             architecture and driver-authoring guides.
vendor/           fetched QEMU binaries (gitignored).
```

## Where this actually is

Working, with 78 tests and clippy clean:

- **Real firmware boots.** A PURR OS T-Deck Plus build gets through the ROM,
  the second-stage bootloader, our partition table, all seven segment loads,
  octal PSRAM detection, and into `app_init`.
- Firmware identification: merged images, bare app images, bootloaders, ELFs.
  Extracts chip, flash size, project name, version, and IDF version.
- Flash assembly, with partition tables validated rather than trusted.
- Launching and controlling QEMU: serial, QMP reset, clean shutdown.
- The application: drag-and-drop, board picker, serial console, and a detached
  serial terminal in its own window.
- The peripheral contract, address routing, external-driver protocol, tracer.

Try it:

```sh
scripts/fetch-qemu.sh
cargo run -p shell            # then drop a firmware .bin on the window
```

Not yet built:

- **GP SPI and I²C controllers in QEMU.** This is the gate on everything
  visual. Boot currently stops right after eFuse init, where the firmware
  first talks to the display.
- Every device driver: display, touch, keyboard, SD card, LoRa, GPS.
- Wi-Fi.

### What the QEMU work actually involves

Espressif's fork already models `SPI_MEM`, the memory-SPI controller that
drives flash and PSRAM, and instantiates it as `spi1`. But the S3's
general-purpose SPI2/SPI3 are a *different peripheral with a different register
map*, and no GP-SPI model exists in the tree for any chip. So SPI2 has to be
written against the TRM. The existing models still help: they supply the QEMU
plumbing — SSI bus integration, chip-select GPIO wiring, MMIO registration —
which is the fiddly part.

### On Wi-Fi

Espressif's QEMU does not emulate the Wi-Fi radio, and the driver is a closed
blob (`libnet80211.a`, `libpp.a`) talking to undocumented registers. The one
serious attempt at register-level emulation
([esp32-open-mac](https://github.com/esp32-open-mac/qemu)) targets the original
ESP32, not the S3.

So the plan is not to emulate the radio. Those blobs are *prebuilt binaries
Espressif ships*, which means `esp_wifi_scan_start` and friends have
byte-identical machine code in every firmware built with a given IDF version —
and the app descriptor tells us that version. We locate those entry points and
trampoline them to emulator-provided implementations, so firmware sees the
simulated APs without executing a single instruction of the real driver.

Scan first, since it needs no TX path, no crypto, and no association. Unknown
IDF version means no Wi-Fi and everything else still boots. It sits behind a
`WifiBackend` trait, so a register-level backend can replace it later without
disturbing anything above.

## Building

Requires Rust 1.82+, and on Windows the MSVC C++ build tools.

```sh
cargo test
cargo clippy --all-targets
```
