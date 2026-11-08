# 15. Building on Linux

## Short version

- **Nothing in this chapter has been run.** No Linux computer was used for this project's builds or for writing this page. What follows is what the repository's scripts and the program's dependencies say, so that someone with a Linux machine knows where to start and what to report.
- The Rust program has code paths for both X11 and Wayland (`eframe` with the `x11` and `wayland` features), and a plain `cargo build` is the expected route.
- The patched QEMU should build with the same `qemu/build.sh`, using the distribution's packages for its libraries. This is expected from how QEMU normally builds on Linux; it has not been tried with our edits.
- Espressif publishes a stock QEMU for Linux (x86_64 and aarch64), which `scripts/fetch-qemu.sh` downloads. It cannot run these boards (see [chapter 13](13-the-patched-qemu.md)), so it is useful only to check that the download works.

## What it is for

The SDK kit has an emulator on macOS only ([chapter 12](12-purr-run-and-the-sdk-kit.md)). Linux is the next obvious place. This page is the list of things to try, in order, and the places where this repository is known to be written with another system in mind.

## The files

| File | Linux-relevant fact |
|---|---|
| [Cargo.toml](../Cargo.toml), [crates/shell/Cargo.toml](../crates/shell/Cargo.toml) | `rust-version = "1.82"`; `eframe` with `glow`, `x11`, `wayland` |
| [scripts/fetch-qemu.sh](../scripts/fetch-qemu.sh) | Knows `x86_64-linux-gnu` and `aarch64-linux-gnu` |
| [qemu/build.sh](../qemu/build.sh) | Applies our files and builds; no Linux-specific branch. `--enable-slirp` unless on Windows |
| [qemu/apply.sh](../qemu/apply.sh) | Written to run with the BSD tools on macOS and MSYS2's on Windows, so it should also run with GNU's. Not run on GNU tools |
| [scripts/build-macos-deps.sh](../scripts/build-macos-deps.sh) | **macOS only**: it passes `-mmacosx-version-min`. Do not use it on Linux |
| [scripts/run-emu.sh](../scripts/run-emu.sh) | Boots an image with QEMU directly; looks for `qemu-system-xtensa` and then `.exe` |

## How it works

On Linux there is no "minimum version" problem of the kind in [chapter 14](14-building-on-macos.md); instead the build machine's C library decides the oldest system the binary runs on. A binary built on a new distribution may not start on an old one. This repository has no policy for that yet.

## How to build and test it

The route to try, in order. **None of it has been run.**

1. Install the build tools. The names below are the usual Debian/Ubuntu packages for QEMU's requirements, and are an expectation, not a tested list: `build-essential git curl python3 python3-venv meson ninja-build pkg-config libglib2.0-dev libpixman-1-dev libslirp-dev libgcrypt20-dev`. QEMU needs Python 3.11 or newer for its configure step, or the `tomli` package.
2. `scripts/fetch-qemu-src.sh`, then `qemu/build.sh`. The result lands in `vendor/qemu/bin/qemu-system-xtensa`.
3. `cargo build --release -p shell`. The window will need the system's graphics libraries (OpenGL and X11 or Wayland); which packages exactly is not known.
4. `cargo test --workspace`, then `ESP32_EMULATOR_REQUIRE_QEMU=1 cargo test -p qemuctl`.
5. Run a flash image as in the README, or headless as in [chapter 10](10-headless-and-trace.md).

If you try it, the useful things to write down are: the distribution and version, the Rust version, which packages the configure step asked for, and whether the window opens.

## What goes wrong

Unknown. Expected, from the other platforms:

| Symptom | Likely cause |
|---|---|
| `'gcrypt.h' file not found` | `libgcrypt` development files missing; the fork needs them on every system |
| `found no usable tomli` | Python older than 3.11 |
| The window fails to open | Missing OpenGL or windowing libraries |
| `qemu-system-xtensa` not found | Not in `vendor/qemu/bin/`; set `ESP32_EMULATOR_QEMU` |

## What is not finished

- Everything above is **untested**.
- A policy for the oldest Linux the binary should run on.
- A packaging story (static build, AppImage, or distribution packages).
- Whether `apply.sh` and `build.sh` run unchanged under GNU `sed`, `awk` and `bash`.
