# Architecture

How the emulator is put together, and why it is put together that way.

Read this before changing anything structural. Most of the decisions here were
made in response to a specific problem, and the problem is recorded alongside
the decision so you can tell when it no longer applies.

---

## 1. The one big idea

QEMU emulates the chip. We emulate everything plugged into it.

```text
    ┌───────────────────────────────┐        ┌─────────────────────────────────┐
    │ QEMU (Espressif fork, in C)   │        │ Emulator shell (Rust)           │
    │                               │        │                                 │
    │  Xtensa cores, RAM, PSRAM     │        │   Registry ── routes by Claim   │
    │  flash, UART, timers, eFuse   │        │      │                          │
    │  SAR ADC  ← we added this     │        │      ├── ST7789    (display)    │
    │                               │        │      ├── GT911     (touch)      │
    │  ┌─────────────────────────┐  │  vpb   │      ├── SD card               │
    │  │ SPI / I²C controllers   │──┼────────┼─────▶├── keyboard              │
    │  │ ← we add these          │  │ socket │      └── your driver ──┐        │
    │  └─────────────────────────┘  │        │                        │        │
    └───────────────────────────────┘        └────────────────────────┼────────┘
                                                                      │
                                              a separate process, any language
```

The split is not arbitrary. Espressif's QEMU models the SoC well but models
**none** of the chips hanging off it, and on the ESP32-S3 it does not even
provide the general-purpose SPI and I²C *controllers* those chips would attach
to. Every T-Deck peripheral sits on one of those two buses.

So we add the bus controllers in C, where they have to be, and implement the
devices in Rust, where they are pleasant to iterate on. A display driver
becomes a Rust file you can unit test, not a C file that needs a 20-minute
QEMU rebuild.

That split is also what makes third-party hardware cheap. A device is just
"whatever answers for a bus address". Whether it lives in this repo or in
someone else's Python script is invisible to everything upstream.

---

## 2. The crates, in dependency order

Each has one job. None of them know about the UI except `shell`.

### `flashimg` — firmware binary formats

Zero dependencies, deliberately. This is pure binary-format work and should
never break because an ecosystem crate churned.

It answers "what is this file?" for anything dropped on the window: a merged
flash image, a bare app image, a bootloader, or an ELF. It reads the ESP image
header, the `esp_app_desc_t` build metadata, and the partition table, then
assembles a bootable flash image.

Two details worth knowing:

- **Blank flash is `0xFF`, not zero.** Erased NOR flash reads as ones, and the
  ROM bootloader relies on that to recognise unwritten regions. Filling with
  zeroes produces an image that fails in a way that looks like a corrupt build.
- **`idf_ver` is load-bearing.** The app descriptor records which ESP-IDF built
  the firmware. That string is the key the Wi-Fi work depends on — see §6.

It also contains a small ELF32 symbol reader, used to turn an address into a
function name. That is not a debugging luxury; it is the same lookup the Wi-Fi
trampoline uses when a build ships its `.elf`.

### `vpb` — the virtual peripheral bus

The contract everything else agrees on. Four ideas:

- **`Claim`** — a bus address a device owns: a SPI chip select, an I²C address,
  a UART, a GPIO. Mirrors how real hardware is addressed.
- **`Transaction`** — something the SoC did, routed to whichever device claimed
  the address.
- **`Peripheral`** — the trait a device implements.
- **`Registry`** — the router.

Plus a wire protocol so a device can live in another process, and a bus tracer.

### `boards` — board definitions

Parses the TOML in `boards/` into resolved bus addresses. A peripheral names a
bus by id; that resolves to a controller index. The T-Deck's shared SPI bus
comes out as three distinct chip selects rather than three devices fighting
over one.

### `qemuctl` — process control

Locates a QEMU binary, builds its command line, spawns it, and talks to it over
QMP. Argument construction is a pure function so the command line can be tested
without spawning anything.

### `shell` — the application

egui. Drag-and-drop, board picker, serial console, and a detached serial
terminal in its own OS window.

---

## 3. How a transaction flows

Firmware writes a pixel to the display. What happens:

1. Firmware writes the SPI controller's registers and sets the start bit.
2. Our QEMU SPI model sees the start bit, gathers the bytes and the chip
   select, and sends one `Transaction::SpiTransfer` over the vpb socket.
3. `Registry::dispatch` looks up which device claimed
   `Claim::Spi { controller: 2, cs: 12 }`.
4. The ST7789 driver decodes it and updates its framebuffer.
5. The UI blits that framebuffer next frame.

### Why a whole transfer, not a byte

A full 320×240 frame is 150 KiB. Round-tripping each byte to another process
would be unusably slow. QEMU batches everything between chip-select edges into
one message.

### Why write-only transfers get no reply

`Transaction::expects_reply` is false for a SPI transfer that reads nothing.
Those are fire-and-forget: QEMU does not wait. Pixel writes are almost all of
the traffic and almost all write-only, so this is the difference between a
working display and a slideshow.

### Why unclaimed I²C addresses NACK

Returning zeroes would tell probing firmware that *every* address holds a
device. Real hardware NACKs, so we NACK. This matters more than it sounds:
scanning the I²C bus is how a lot of firmware discovers its own hardware.

### Why a claim collision is an error

Two devices on one address is refused at registration. Silently letting the
first one win produces bugs that look like firmware faults, and you will spend
an afternoon on it.

---

## 4. Three ways to add hardware

Covered in detail in [custom-hardware.md](custom-hardware.md). In short:

| | Approach | Rebuild? | Good for |
|---|---|---|---|
| 1 | Edit a board TOML | none | panel size, moved pin, new address |
| 2 | Implement `Peripheral` | emulator | devices we ship for everyone |
| 3 | Speak the wire protocol | none | your own hardware, any language |

Reach for the lowest number that works.

---

## 5. Mimic touch

The host has a mouse; the board has a touch panel. `vpb::input` converts one
into the other, and it is more than passing coordinates through:

- **Rotation is undone.** The displayed image is already rotated to match how
  the panel is mounted, so a tap has to be rotated back into panel space or it
  lands somewhere else.
- **Scaling is undone.** The window is usually bigger than 320×240.
- **Press and release are distinct.** A touch controller reports a sustained
  press, not a click. Firmware that debounces needs both edges.
- **A fast click cannot be missed.** Touch controllers are *polled*. If a press
  and release both happen between two polls, a naive implementation reports
  nothing. So a release is latched until a driver consumes it.
- **A hover is not a touch.** Mouse movement with no button held is dropped;
  no touch panel can report a finger that is not touching.

The module has no UI dependency, so it is unit tested and can be driven from a
script as easily as from a mouse.

---

## 6. Wi-Fi, and why it is not emulated

Espressif's QEMU does not emulate the Wi-Fi radio. The driver is a closed blob
(`libnet80211.a`, `libpp.a`) talking to undocumented registers, and the one
serious attempt at register-level emulation,
[esp32-open-mac](https://github.com/esp32-open-mac/qemu), targets the original
ESP32 rather than the S3.

The plan is to not emulate the radio at all.

Those blobs are **prebuilt binaries Espressif ships**, not compiled per
project. So `esp_wifi_scan_start` and friends have byte-identical machine code
in every firmware built with a given IDF version — and the app descriptor tells
us that version. We locate those entry points and trampoline them to
emulator-provided implementations. Firmware sees the simulated access points
without executing a single instruction of the real driver.

Scanning first, because it needs no transmit path, no crypto, and no
association. An unknown IDF version means no Wi-Fi and everything else still
boots. It sits behind a `WifiBackend` trait so a register-level backend could
replace it later without disturbing anything above.

---

## 7. What actually works today

Real firmware boots. A PURR OS T-Deck Plus build runs through the ROM, the
second-stage bootloader, our partition table, seven segment loads, octal PSRAM
detection, and into `app_init`.

Getting there required one non-obvious fix in each half of the system:

- **PSRAM** needs *two* QEMU settings that each do nothing alone: `-m 8M` for
  size and a global to switch the modelled chip to octal. Without both, the
  T-Deck calls `abort()` during startup and boot-loops.
- **The SAR ADC** did not exist at all, so firmware polling for conversion
  completion spun forever. See [qemu/README.md](../qemu/README.md).

Not built yet: the SPI and I²C controllers, every device driver, and Wi-Fi.

---

## 8. Debugging tools

When firmware stops producing output, these answer why.

```sh
# What is this file?
cargo run -p flashimg --example inspect -- firmware.bin

# Where did it stop? Samples the PC, disassembles, checks both cores.
cargo run -p qemuctl --example probe -- flash.bin 15

# Turn an address into a function name.
cargo run -p flashimg --example addr2sym -- build/app.elf 0x42119853
```

Together these found the ADC stall: `probe` showed both cores parked on one
instruction, disassembly showed a register poll, the address register named the
peripheral, and `addr2sym` confirmed the calling function.

**Two warnings from experience.**

Check that the ELF matches the image you are running. Comparing the ELF's
SHA-256 against the `app_elf_sha256` in the app descriptor takes a second, and
symbolising against a *different* build of the same firmware yields plausible,
confidently wrong answers.

And read the `[GUESS]` marker. When no symbol's declared size covers an
address, `addr2sym` falls back to the nearest preceding function and says so.
Without that marker a guess is indistinguishable from a hit, and you go and
debug a function the CPU was never in. Even an *exact* hit can mislead if a
symbol's size overruns into an unnamed neighbour — one stall here resolved
confidently to `esp_cpu_unstall`, and disassembly showed the real code was a
two-instruction `waiti` parked in the function next door. Disassemble before
believing a symbol.

### Measuring instead of guessing

The strongest tool is not in this repo. PURR OS ships a **hardware probe**
firmware exposing peek/poke, GPIO, traced SPI transactions, interrupt
characterisation and DMA over its USB console, with an allowlist gate that
makes eFuse unreachable. Pointed at a real T-Deck it supplies the *read* side
of hardware behaviour — what registers return — which an instrumented driver
cannot capture and which an emulator has to invent.

Five behaviours in the SPI controller came from it directly, replacing
guesses, and one of those guesses was wrong in a way no amount of reasoning
would have caught. Where a measurement exists, it is cited at the code.

The bus tracer is the other half of this, once buses exist. It is off by
default, has a switch per bus, and lets devices decode their own traffic — so
you see `CASET x=0..319` rather than a hex dump, which is the difference
between confirming bytes moved and confirming the *right* bytes moved.
