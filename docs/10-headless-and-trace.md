# 10. Running without a window, and the bus trace

## Short version

- Two programs run the emulator with no window: `headless` and `headless_tap`. Both are Cargo examples of the `shell` crate. `headless_tap` is the one to use for scripts: it can press the screen, drag, type and take pictures while it runs.
- A script drives `headless_tap` by appending one command per line to a file named in `HEADLESS_CMD`.
- `VPB_TRACE=i2c` (or `spi`, `uart`, `gpio`, comma separated) turns on the bus tracer, and `VPB_TRACE_FILE` says where the records go. `ESP32_QEMU_ARGS='-d guest_errors,unimp -D qemu.log'` passes flags to QEMU for its own device logs.
- Both programs work on a **copy** of the flash image, in a folder under the system temp directory. The file you pass is not changed.
- Tested on a Mac with an Intel processor. Nothing here has been run on Linux or Windows.

## What it is for

A window has to be watched. A script cannot do that, and a failure that shows up only after a tap, or only after the screen's idle switch-off, is not reproducible by eye. The headless runners boot the same code path as the window (`Session` in `crates/shell/src/session.rs`, the same board file, the same device models), so what a script sees is what the window would show.

## The files

| File | What it is |
|---|---|
| [crates/shell/examples/headless.rs](../crates/shell/examples/headless.rs) | Boots a firmware, optionally types a string or one key, sends serial lines, takes pictures. Prints the bus trace to standard error |
| [crates/shell/examples/headless_tap.rs](../crates/shell/examples/headless_tap.rs) | The same, plus taps, drags and a command file read while it runs. Writes the bus trace to a file |
| [crates/shell/src/session.rs](../crates/shell/src/session.rs) | Starting QEMU, the serial log, the extra QEMU arguments |
| [crates/vpb/src/trace.rs](../crates/vpb/src/trace.rs) | The tracer: which buses, how many bytes per record |
| [crates/devices/src/](../crates/devices/src/) | Each device decodes its own traffic into the text that the trace prints |

## How it works

### Starting

```sh
BOARD_TOML=boards/t-deck-plus-vm.toml \
  cargo run --release -p shell --example headless_tap -- <flash.bin> [seconds] [out.png]
```

`BOARD_TOML` is required for `headless_tap`. `seconds` defaults to 120. When the time is up it writes `out.png` (the screen at that moment), prints the serial log, and prints QEMU's standard error if there was any. `headless` has the same job for a board with a default (the T-Deck Plus) and also takes an SD card image: `headless <flash.bin> [seconds] [sd.img] [out.png]`.

The working copy of the flash image is `esp32-emulator-headless-tap/flash.bin` (or `esp32-emulator-headless/` for `headless`) under the system temp directory. The folder is the same for every run on the computer, so a second run overwrites the first. Set `TMPDIR` to give a run its own folder, and the guest's writes (accounts, settings) stay readable afterwards in that copy.

### The command file

Set `HEADLESS_CMD=<file>`. The program re-reads the file as it runs and executes each **new** line once.

| Command | What it does |
|---|---|
| `tap X Y` | Press the screen at X, Y for 300 ms, then release. The screen is 320 by 240, landscape, as you see it |
| `drag X1 Y1 X2 Y2` | Press, slide in 8 steps, release |
| `type TEXT` | Press keys on the keyboard model, one every 150 ms. `\n` is Enter |
| `serial TEXT` | Send `TEXT` and a carriage return to the first serial port |
| `shot NAME` | Write `<out>-NAME.png` |
| `dump` | Write `<out>-serial.txt`, everything the board has printed so far |
| `module KIND` | Plug a module into the board's slot while running (`gamepad`, `tdeck-keyboard`, or `none`). The guest sees it the next time it probes |
| `pad press\|release\|tap NAME` | Hold, let go of, or tap a game-pad button (`up down left right a b x y func`). Prints which LEDs the guest has lit |
| `tilt X Y Z` | Set the motion sensor's acceleration, in g (flat is `0 0 1`) |
| `quit` | Stop; the final picture and the serial log are still written |

The program does no waiting between commands. The emulated board is slower than the clock, so the caller paces them: a tap that opens an app needs seconds before the next one.

`type` prints how many keys it queued and how many the guest had not read yet. `type` goes to the keyboard model (an I²C device at 0x55 on the T-Deck boards), not to a text field and not to the serial port. A prompt on the serial console needs `serial` instead.

### Environment variables

| Variable | Used by | Effect |
|---|---|---|
| `BOARD_TOML` | both (required for `headless_tap`) | The board file |
| `HEADLESS_CMD` | `headless_tap` | The command file |
| `HEADLESS_MODULE` | `headless_tap` | What is plugged into the board's slot at boot (`gamepad`, `tdeck-keyboard`, `none`); the board's default if unset |
| `HEADLESS_TAPS` | `headless_tap` | `second:x:y,second:x:y` taps on a schedule |
| `HEADLESS_SHOTS` | both | Seconds at which to write `<out>-<second>.png` |
| `HEADLESS_SERIAL`, `HEADLESS_SERIAL_AT` | both | Lines (`a\nb\n`) sent to serial from that second |
| `HEADLESS_SERIAL_PORT`, `HEADLESS_SERIAL_GAP` | `headless` | Which serial port; seconds between lines |
| `HEADLESS_KEY`, `HEADLESS_KEY_AT` | `headless` | One key at a second |
| `HEADLESS_TYPE`, `HEADLESS_TYPE_AT` | `headless` | A string typed, one key every 150 ms |
| `VPB_TRACE` | both | `i2c`, `spi`, `uart`, `gpio`, comma separated |
| `VPB_TRACE_FILE` | `headless_tap` | Where trace records go (default `vpb-trace.log` in the current folder) |
| `ESP32_QEMU_ARGS` | all sessions, window too | Extra QEMU arguments, split on spaces |
| `ESP32_SERIAL_LOG` | all sessions | A file that receives every serial byte |
| `ESP32_EFUSE` | all sessions | An eFuse image file |
| `ESP32_EMULATOR_QEMU` | all sessions | A QEMU binary, or the folder holding it, instead of the one in `vendor/` |

### The bus trace

`headless_tap` writes one line per transaction, stamped with seconds since boot so it lines up with the serial log and with your `tap` lines. An I²C read of the keyboard looks like `poll (idle)` when nothing is pending and `key 'a'` when a key was delivered. The touch panel decodes as `seek STATUS`, `read POINT_0` and so on. An SPI trace includes every framebuffer push and is very large; trace `i2c` unless you need the rest.

QEMU's own view is separate: `ESP32_QEMU_ARGS='-d guest_errors,unimp -D qemu.log'` writes the I²C controller's register-level lines (`i2c0: irq raw=…`, `i2c0: R 5d n=…`) and any guest access QEMU rejects. Without `-D` they go to QEMU's standard error, which the runner prints at the end. Keep both files when chasing a bug; neither shows what the other does.

## How to build and test it

```sh
cargo build --release -p shell --example headless_tap     # target/release/examples/headless_tap
cargo test --workspace                                    # no QEMU needed
ESP32_EMULATOR_REQUIRE_QEMU=1 cargo test -p qemuctl       # the boot test, with the patched QEMU in vendor/
```

Without `ESP32_EMULATOR_REQUIRE_QEMU` the QEMU boot test skips itself when no QEMU is found.

## What goes wrong

| Symptom | Likely cause |
|---|---|
| `set BOARD_TOML to a board file` | `headless_tap` has no default board |
| A run does nothing after the command file is written | The runner reads the file as it loops; lines already counted are not re-run, so replacing the file with a shorter one skips commands. Append, do not rewrite |
| `type` has no visible effect | `type: N keys … M still unread` says whether the guest read them. If it never reads, check for the keyboard at 0x55 in an `i2c` trace. If the prompt is on serial, use `serial` |
| Two runs fight over the working copy | They share `esp32-emulator-headless-tap/` under the temp directory. Give each its own `TMPDIR` |
| A tap right after the screen's idle switch-off crashes the guest | A report, not a diagnosis. The I²C model returns 0xFF for a read of an empty FIFO and never faults the bus, so the cause is not known to be on the emulator's side. Capture an `i2c` trace and a `-d guest_errors,unimp` log |
| A large `spi` trace fills the disk | Trace `i2c`, or stop the run sooner |

## What is not finished

- **Untested** on Linux, Windows and Apple Silicon.
- `headless_tap` takes no SD card image argument; `headless` does.
- The command file is polled every 50 ms; commands are not timestamped, so a long script cannot ask for "at second 40".
- There is no command to read the screen as text, to wait for a serial line, or to release a held key.
- The GPIO interrupt lines named in a board file (`irq = 16`, `irq = 46`) are not driven into the guest. A driver that waits for one of them would not see key or touch events.
