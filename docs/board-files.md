# Board files

A board is a TOML file. Adding support for a new one is usually an edit, not a
code change.

Drop a `.toml` on the emulator window to load it. Re-dropping the same file
replaces it in place, so you can keep the editor open and iterate.

---

## A minimal board

Enough to boot to a serial console:

```toml
[board]
id = "my-board"
name = "My Board"
chip = "esp32s3"
flash_size = "4MB"
```

That is genuinely all. Everything else describes hardware hanging off the chip.

---

## `[board]`

| Key | Required | Notes |
|---|---|---|
| `id` | yes | Stable identifier. Re-dropping a file with the same id replaces it. |
| `name` | yes | Shown in the picker. |
| `chip` | yes | `esp32`, `esp32s3`, `esp32c3`, … |
| `flash_size` | yes | `16MB`, `4M`, `8 MiB`, or a raw byte count. |
| `default_for_chip` | no | Fallback profile when a firmware's board is unknown. |

`chip` accepts the spellings people actually write: `esp32s3`, `ESP32-S3`,
`esp32_s3`, `s3`. Only ESP32, ESP32-S3 and ESP32-C3 have QEMU machines today;
the rest parse fine and are rejected at launch with a clear message.

### `[board.psram]`

```toml
[board.psram]
kind = "octal"   # none | quad | octal
size = "8MB"
```

**Get this right.** Boards that put their heap in PSRAM have firmware that
calls `abort()` when it is missing, so a wrong value is a boot loop rather
than a degraded experience. The T-Deck is one of them.

---

## `[[bus]]`

Each bus the SoC drives.

```toml
[[bus]]
id = "fspi"        # referenced by peripherals
kind = "spi"       # spi | i2c | uart | i2s
controller = 2     # SPI2, I2C0, UART1 …
sck = 40
mosi = 41
miso = 38
```

I²C uses `sda`/`scl`; UART uses `tx`/`rx`. Any integer key is kept as a pin.

**The controller number is what matters.** Pins are documentation for humans
and for the tracer; routing happens on `controller` plus chip select or
address. Two devices on the same controller with the same chip select is a
hard error, not a warning.

---

## `[[peripheral]]`

Everything attached to a bus.

```toml
[[peripheral]]
kind = "st7789"
label = "2.8in LCD"     # shown in the UI; defaults to kind
bus = "fspi"            # a [[bus]] id
cs = 12                 # GPIO the chip select comes out on
cs_line = 0             # the controller's CS index — see below
dc = 11                 # data/command; without it a display cannot be decoded
backlight = 42
width = 320
height = 240
rotation = 90
invert = true           # the glass is wired inverted, not the register
enabled = true          # default true
```

### `cs` and `cs_line` are different numbers

`cs` is the pin, which is what a schematic gives you and what the emulated SoC
needs for routing. `cs_line` is the chip-select line the SPI controller
drives, 0..5, and that is what decides which device answers a transaction.

They rarely match. ESP-IDF assigns the line at runtime through the GPIO
matrix, so it cannot be worked out from the board: on a T-Deck the display is
GPIO 12 on line 0, and the SD card GPIO 39 on line 5. The values in `boards/`
were read off a bus trace of the real driver.

Leaving `cs_line` out falls back to `cs`, which keeps older files loading and
is correct only where the two happen to coincide. If a device never answers,
this is the first thing to check — turn the SPI tracer on and see which line
the traffic is actually arriving at.

For I²C, use `address` and optionally `alt_address` — several controllers are
strapped to one of two addresses, and the GT911 is one:

```toml
[[peripheral]]
kind = "gt911"
bus = "i2c0"
address = 0x5d
alt_address = 0x14
irq = 16
```

### Touch panels have two resolutions, and they disagree

A touch controller *advertises* a resolution in a register, and separately
*emits* coordinates in some range. Those are usually the same number. On a
panel mounted sideways they are not, and the difference is the single most
confusing bug in this whole project to diagnose:

```toml
# What the chip reports at X_RESOLUTION, which drivers read.
width = 320
height = 240
# What its point registers actually span, in the panel's own frame.
point_width = 312
point_height = 236
rotation = 270
```

Both `point_*` keys default to `width`/`height`, and `rotation` to `0`, so a
board you have not measured behaves as before.

Get this wrong and **the centre of the screen still works**. Swapping the two
axes and flipping one leaves the midpoint almost exactly where it started, so
the first gesture anybody tries — tap the middle to unlock — succeeds, and
every other tap lands somewhere else. It reads as "touch is flaky" rather than
"touch is systematically wrong", which sends you looking in the wrong place.

The numbers are not derivable from a datasheet. A T-Deck's GT911 spans
`0..=311` by `0..=235`, a quarter turn from the display, and the only way to
know that is to read what the firmware divides by, or measure a real panel.

### `kind` is not a fixed list

An unrecognised `kind` is **not** an error. It may be served by an external
driver that registers for the same address at runtime — see
[custom-hardware.md](custom-hardware.md). Validation checks structure (do bus
references resolve, do two devices collide), not whether we happen to ship a
driver.

### Unknown keys are reported

Free-form config has one bad failure mode: a typo does nothing at all, and you
are left wondering why the display is blank. So every key a driver reads is
tracked, and the rest are surfaced as warnings in the UI.

Writing `wdith = 320` gets you `st7789: unrecognised key "wdith"` rather than
silence.

### Not-connected pins

Omit the key. Do **not** write `-1`; it is rejected with a message telling you
to omit it. `-1` would otherwise wrap into a valid-looking GPIO number.

---

## Rotation and touch

`rotation` on a display is in degrees (`0`, `90`, `180`, `270`) and describes
how the panel is mounted relative to its native orientation.

It also drives touch. The displayed image is rotated to match the mounting, so
a click has to be rotated *back* into panel coordinates or taps land somewhere
else. Setting `rotation` on the display is what makes mimic touch line up —
there is no separate touch rotation key, deliberately, because two settings
that must agree is two settings that will disagree.

---

## `[support]`

Optional, honest bookkeeping. Surfaced in the UI so it is clear what is
actually emulated:

```toml
[support]
implemented = []
planned = ["st7789", "gt911", "sdcard"]
stubbed = ["sx1262", "gnss"]
```

---

## Shipped boards

| File | Board | Chip |
|---|---|---|
| `t-deck-plus.toml` | LilyGO T-Deck Plus | ESP32-S3, 16MB, 8MB octal PSRAM |
| `cyd-esp32-2432s028r.toml` | CYD 2.8" resistive | ESP32, 4MB, no PSRAM |
| `cyd-s024c.toml` | CYD 2.4" capacitive | ESP32, 4MB, no PSRAM |
| `cyd-s028r.toml` | CYD 2.8" flipped panel | ESP32, 4MB, no PSRAM |
| `generic-esp32s3.toml` | Fallback devkit | ESP32-S3, 4MB |

These are parsed by the test suite, so a broken board file fails CI rather than
failing at run time.

---

## Two things to watch for

**A board file describes hardware, not firmware.** When PURR OS's
`devices/cyd/device.pcat` and the published ESP32-2432S028R pinout disagreed
about XPT2046 touch wiring, the board file followed the hardware. A firmware
that drives touch on the wrong pins then shows up in the tracer as traffic to
an address nothing owns — which is exactly the signal you want.

**Bit-banged buses are invisible.** We intercept SPI and I²C at the
*controller*. A firmware that toggles GPIOs by hand produces no bus
transactions to route, so its device will not work no matter how correct the
board file is. Reconstructing a bus from GPIO edges is possible but not
implemented.
