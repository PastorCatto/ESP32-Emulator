# 12. How `purr run` and the SDK kit use the emulator

## Short version

- The PURR SDK kit is a separate project (in the PURR OS repository, folder `sdk/`). It carries a built copy of this emulator in its `emulator/` folder.
- `purr emulator [image]` opens the **window** on a **copy** of a VM image. The original is not changed.
- `purr run <app.cat>` builds a one-app repository, serves it from a local server on port 47801, boots the **headless** emulator on a VM image, and drives it through `headless_tap` with a scripted flow: unlock, Developer Portal, Find servers, Trust, scroll, Install.
- It was checked end to end on an Android-shell image up to "Install" (the app showed "installed"). **Launching the installed app by script is not done.**
- The kit has no VM image and no published test key. A VM image needs a trust block signed with the boot key, which only the owner can make, so `purr run` works with an image you supply and an app signed with a key that image trusts.
- On Windows the kit has no emulator: `emulator.find()` returns nothing there, because the patched QEMU is not built for Windows ([chapter 16](16-building-on-windows.md)).

This page is written from the PURR OS session's description of its own code and from reading `sdk/purrsdk/emulator.py` on the same computer. Nothing on it was run for this page.

## What it is for

An app developer should not need a T-Deck on the desk. The SDK builds and signs an app, and the emulator is where it is put on a device image and looked at. This chapter is the emulator's side of that: what the SDK expects to find, what it calls, and where the two projects meet.

## The files

| In this repository | In the PURR OS repository (`sdk/`) |
|---|---|
| `crates/shell/examples/headless_tap.rs`: the headless runner the SDK drives | `purrsdk/emulator.py`: finds the emulator, starts the window, runs the headless flow |
| `crates/shell/src/main.rs`: the window (`esp32-emulator`) | `docs/12-emulator.md`: the kit's page for developers |
| `boards/t-deck-plus-vm.toml`: the board `purr run` uses | `docs/13-first-hour.md`: the first-hour walk-through |
| `qemu/`, `scripts/`: how the patched QEMU is built | `build-kit.py`: copies the emulator into the kit; `NOTICES-emulator.md`: its notices |

## How it works

### Where the SDK looks for the emulator

`emulator.find()` looks, in order, in: the folder named by `$PURR_EMULATOR_DIR`; the kit's `emulator/` folder; a checkout of this repository. `$PURR_EMULATOR` names a launcher for the window. In each folder it looks for `run-emulator` or `esp32-emulator` (the window) and `headless_tap`, in the folder itself, `bin/`, `target/release/`, `target/release/examples/`, `target/debug/` and `target/debug/examples/`. It looks for the boards in `boards/` next to them.

`run-emulator` is the kit's launcher script, written by the SDK's `build-kit.py`. It is **not in this repository**.

### What it runs

| `purr` command | Runs | Board | Image |
|---|---|---|---|
| `purr emulator [image]` | the window, on a copy of the image in a temporary folder | `t-deck-plus-vm` | the argument, `$PURR_VM_IMAGE`, or the kit's `images/vm-flash.bin` |
| `purr run <app.cat>` | `headless_tap` with `BOARD_TOML`, `HEADLESS_CMD`, and a `TMPDIR` of its own, in a new process group so that QEMU is stopped with it | `t-deck-plus-vm` | the same |

For `purr run` the SDK also publishes the app into a throwaway repository, starts its dev server on port 47801, and appends commands to the command file ([chapter 10](10-headless-and-trace.md)): `tap`, `drag`, `shot`, `dump`. It waits on the board's serial output for known lines (`android-home:` for the Android-style shell) and paces the commands by the clock.

The guest reaches the computer running the emulator at **10.0.2.2** (QEMU's user-mode network). The VM board's W5500 Ethernet model carries that traffic ([vm-hardware.md](vm-hardware.md)). The network only goes out: the computer cannot dial into the guest.

### The scripted flow

The coordinates and waits in the SDK were worked out by hand on the 1.0.27 Android-shell image. They are tied to that shell's screens; another shell needs its own steps, and `purr emulator` (the window) is the way to do it by eye. The SDK's code notes that the second half of the flow (the Developer Portal steps) was worked step by step by hand, and that a press right after the panel's idle switch-off had crashed the guest.

## How to build and test it

From this repository's side, the kit needs two programs and the patched QEMU:

```sh
cargo build --release --locked -p shell --bin esp32-emulator --example headless_tap
```

(the same command `build-kit.py` runs). Then the SDK is pointed at the result with `PURR_EMULATOR_DIR`, or the kit is built with `python3 sdk/build-kit.py` (see the SDK's own documentation; **not run for this page**).

A quick check that the headless side works without the SDK is [chapter 10](10-headless-and-trace.md).

## What goes wrong

| Symptom | Cause |
|---|---|
| `There is no emulator in this kit` | None of the three places has `esp32-emulator`/`run-emulator` or `headless_tap`; or the computer is Windows |
| `I need a flash image to boot` | No image argument, no `$PURR_VM_IMAGE`, no `images/vm-flash.bin` in the kit |
| The guest cannot reach the dev server | Port 47801 already in use (`purr run` refuses it), or the board has no Ethernet model (only `t-deck-plus-vm` does) |
| The scripted flow taps the wrong thing | A different shell or version than the one the coordinates were made on |
| The guest crashes after a press following the screen's idle switch-off | Seen by the PURR OS session. An `i2c` trace and a `-d guest_errors,unimp` log from a reproducing run are the next step, see [chapter 10](10-headless-and-trace.md). Reading the I²C model gave no way for it to fault the bus |
| Typed text does not appear | The keyboard is an I²C device read by polling; see [chapter 10](10-headless-and-trace.md), `type` |

## What is not finished

- **Launching the installed app by script is not done.**
- No VM image or test key is published, so a developer cannot run `purr run` from the kit alone.
- No emulator in the Windows kit.
- The kit page (`sdk/docs/12-emulator.md`) says the emulator "runs on a Mac with an Intel processor and macOS 26 or newer". The static macOS 12 build ([chapter 14](14-building-on-macos.md)) lowers that floor to 12.0 on paper; it has not been run on any macOS older than 26.
- Untested: Apple Silicon, with the x86_64 build under Rosetta 2.
- The emulator is not a security test: the VM image is signed with developer keys.
