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

Real firmware boots and runs its drivers. A PURR OS T-Deck Plus build gets
through the ROM, the second-stage bootloader, our partition table, seven
segment loads, octal PSRAM detection and `app_init`, then:

- initialises the SD card over SPI — CMD0 through ACMD51, CRC checking
  enabled, against a raw `.img` served by a Rust device model;
- brings up the ST7789, which reports `ST7789 ready 320x240`, switches to
  bulk DMA mode and pushes full 320×240 framebuffers, ten chunks per frame —
  and those pixels are decoded back into an image, so its boot splash comes
  out the other end;
- finds the GT911 touch controller on I²C — `GT911 at 0x5D — ID: 911` — and
  registers it, along with the trackball and the BBQ20 keyboard;
- loads its static modules and reaches Wi-Fi PHY init.

It stops inside `phy_init`, which is where the unmodelled radio begins — see
[section 6](#6-wi-fi-and-why-it-is-not-emulated) and the note below.

### How far register-level modelling gets the PHY, and where it stops

Worth recording, because the answer is "further than expected, and still not
far enough".

The closed PHY blob's first stall was an infinite spin in ROM waiting on the
analog master at `0x6000E050`, which nothing mapped, so it read zero forever.
Modelling that block's completion bits got past it. Then PLL calibration
failed three times and gave up; adding the operation-done bit at `0x6000E04C`
got past that. Then it waited on `SENS_TSENS_READY` — the on-die temperature
sensor, which RF calibration compensates against — and modelling that got past
*that*.

It now stops dead at guest time ~3.7 s with no further output, four PLL
calibration failures behind it, somewhere inside the blob's own calibration.

Each fix revealed the next poll, and the trend is not toward completion: the
PHY is calibrating a radio, and there is no radio. The two device models this
produced are worth keeping either way — the temperature sensor is a real
documented block, and the analog master's handshake is real — but the
remaining path to a working `esp_wifi` is not more of this.

Getting there meant writing four device models that did not exist, and fixing
four bugs that did.

The models, all in [qemu/](../qemu/) and applied to an unmodified release
tarball: **GP SPI2/SPI3** (absent for every chip, so anything on a board's
general-purpose SPI bus was invisible), the **SAR ADC** (firmware polling for
conversion completion spun forever while reading the battery), the **USB
Serial/JTAG console** (a register stub, so firmware using the S3's native USB
booted silently), and the **vpb bridge** itself.

The bugs are written up in [qemu/README.md](../qemu/README.md). Three were in
Espressif's own models and shared a single symptom — SD init hanging, which
held the SPI bus lock and stopped the display from ever starting: a GDMA
channel lookup that matched channels nobody had programmed, the same lookup
reading an RX channel's START bit at the TX offset, and an interrupt matrix
that ignored mapping changes. The fourth was ours: a deliberate delay before
dropping the interrupt line, which re-entered ESP-IDF's SPI ISR after it had
cleared `trans_done` and tripped an assert into a boot loop.

PSRAM needs *two* QEMU settings that do nothing alone: `-m 8M` for size and a
global switching the modelled chip to octal. Without both the T-Deck calls
`abort()` during startup and boot-loops. `scripts/run-emu.sh` sets them.

The shell drives all of that itself: it reads the board file, builds the
device models it has, serves them on a port it picked, tells the emulator
where to find them, and draws the panel live. `cargo run -p shell --example
headless` runs the same path with no window, which is how it gets tested.

Still open: a freshly created `.img` has no filesystem on it, so the card
initialises and then FATFS reports `FR_NO_FILESYSTEM`; a real card image
mounts. The bus tracer works but has no pane of its own in the UI. Wi-Fi is
untouched.

### The S3 renumbered the I²C command opcodes

Worth writing down because it is invisible. The ESP32 and ESP32-S3 use
different values for the same I²C command-list opcodes:

| | RESTART | WRITE | READ | STOP | END |
| --- | --- | --- | --- | --- | --- |
| ESP32 | 0 | 1 | 2 | 3 | 4 |
| ESP32-S3 | **6** | 1 | **3** | **2** | 4 |

A controller built from the ESP32's values compiles, runs every command list
the driver programs, and never drives the bus: restart decodes as an unknown
opcode, and reads and stops trade places. Nothing errors — the transaction
completes, and every address reads as empty.

From `components/hal/esp32s3/include/hal/i2c_ll.h`.

### One connection per controller

The emulator opens a separate vpb socket for each controller: SPI2, SPI3,
I²C0, I²C1. The server therefore has to serve them concurrently — a thread per
connection, sharing the registry behind a mutex. Handling them one at a time
leaves every bus after the first sitting in the accept queue for the life of
the machine, and the symptom is a bus whose traffic never arrives at all.

The lock is taken per transaction rather than per connection, so a framebuffer
push does not hold off a touch poll. Interleaving between buses is harmless:
they are independent, and ordering within one is preserved by each having its
own connection.

### Board files carry two different chip-select numbers

`cs` is the GPIO the chip select comes out on, which is what a schematic gives
you. `cs_line` is the controller's CS index, 0..5, which is what the bus
actually routes on — and they are not the same number. On a T-Deck the display
is GPIO 12 on line 0, and the SD card GPIO 39 on line 5.

The mapping is made at runtime by ESP-IDF through the GPIO matrix, so it is
not derivable from the board; the values in `boards/` came from a bus trace of
the real driver. Matching against the GPIO means no device ever answers, which
looks exactly like a bus that is not working.

### The data/command pin

Worth its own note, because it is the thing that makes a display model
possible at all. An ST7789 tells a command byte from pixel data by a GPIO and
by nothing on the SPI bus itself, so a decoder that only sees the bus cannot
tell `RAMWR` from a mid-grey pixel.

The vendored GPIO model was a stub — it answered `GPIO_STRAP` and dropped
every write — so no pin could be observed at all. It now models the output and
enable latches and drives a line per pin, and the SPI controller latches the
board's D/C pin and carries its level on each transaction. Which pin that is
comes from the board file: 11 on a T-Deck Plus, 2 on a CYD.

The alternative, guessing from transfer length, works right up until a
single-byte *parameter* arrives — `COLMOD 0x55` is indistinguishable from a
command that way. `St7789` still has that fallback for a board file with no
`dc` set, and it is a fallback, not a second supported mode.

---

## 8. Debugging tools

When firmware stops producing output, these answer why.

```sh
# What is this file?
cargo run -p flashimg --example inspect -- firmware.bin

# Boot it, with an SD card attached. Prints where it left the logs.
scripts/run-emu.sh flash.bin 60 sd.img

# The device models' own traces, into qemu.log.
QEMU_DEBUG=unimp,guest_errors scripts/run-emu.sh flash.bin 60 sd.img

# Where did it stop? Samples both cores' registers over the QEMU monitor.
scripts/sample-pc.sh flash.bin 40 8 sd.img

# Turn an address into a function name.
cargo run -p flashimg --example addr2sym -- build/app.elf 0x42119853
```

Everything captures all three serial ports, not just the console — the scripts
and the application both. Firmware does not always log where you expect, and
chasing a "hang" that was really output going to a port nobody was reading
costs an afternoon.

The ports are not interchangeable and neither is redundant. On a T-Deck
running PURR OS:

| Port | Carries |
| --- | --- |
| UART0 | the ROM banner, then the application's own logger — 11.6 KB of a 70s boot |
| UART1 | nothing |
| USB Serial/JTAG | the ROM banner again, then ESP-IDF's console — 4.7 KB |

The ROM writes its banner to UART0 *and* the USB console, so a merged view
shows the whole early boot twice; the application afterwards splits across
them, so a single port shows half. The shell keeps a buffer per port plus a
merged one and lets you pick, defaulting to UART0 as the closest thing to a
complete view. The merged view marks where the source changes.

The emulator dials out to sockets the shell is already listening on, rather
than listening itself, so nothing is lost between QEMU starting and something
attaching — the ROM banner is gone in milliseconds otherwise.

`sample-pc.sh` takes `MON_EXTRA` for one-off monitor commands. Reading an
interrupt matrix mapping with `xp /1wx 0x600c2054` and comparing it against
the CPU's `INTENABLE` and `INTERRUPT` is what identified the matrix bug —
the peripheral was holding its line high and the CPU never saw it.

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
