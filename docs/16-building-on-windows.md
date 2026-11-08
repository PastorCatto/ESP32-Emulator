# 16. Building on Windows

## Short version

- **There is no Windows build of the patched QEMU that can be handed to anyone.** That is the only reason the Windows SDK kit has no emulator.
- This project began on Windows. The notes in [qemu/README.md](../qemu/README.md) were written while building QEMU in an MSYS2 MINGW64 shell, and the scripts still use the `.exe` name. The early history of the repository records it. **None of that has been re-run for this page**, and the Mac used since cannot run it.
- What is **known**: the list of MSYS2 packages that build was written against, and three specific failures with fixes that `apply.sh` carries (below).
- What is **not known**: whether the build completes today with the current `apply.sh`; whether the Rust program builds and runs on Windows today; how the finished QEMU would be packaged (the MSYS2 runtime DLLs it needs, and their licences).
- The SDK side has a second block: `purr run` drives the emulator with POSIX process groups, and `emulator.find()` returns nothing on Windows. Even with a QEMU, the SDK would need changes ([chapter 12](12-purr-run-and-the-sdk-kit.md)).

## What it is for

A Windows developer using the PURR SDK has no emulator today. This page says what a person would have to do, and what has to be established, to change that.

## The files

| File | What it is |
|---|---|
| [qemu/README.md](../qemu/README.md), "Building" and "Three Windows gotchas" | The MSYS2 package list and the three problems |
| [qemu/apply.sh](../qemu/apply.sh) | Carries the fixes for those problems |
| [qemu/build.sh](../qemu/build.sh) | Moves an old build directory aside rather than deleting it (Windows can hold a lock); builds `qemu-system-xtensa.exe` first; turns slirp off on Windows |
| [scripts/run-emu.sh](../scripts/run-emu.sh), [sample-pc.sh](../scripts/sample-pc.sh) | Look for `qemu-system-xtensa` and then `qemu-system-xtensa.exe` |
| [crates/shell/src/main.rs](../crates/shell/src/main.rs) | Hides the console window in release builds on Windows |
| [.gitignore](../.gitignore) | Names old `esp32-emulator-<hash>.exe` builds, which is the evidence that the program was once built for Windows |

## How it works

### The packages (as written down, not re-tried)

From an **MSYS2 MINGW64** shell:

```sh
pacman -S --needed \
  mingw-w64-x86_64-gcc mingw-w64-x86_64-glib2 mingw-w64-x86_64-pixman \
  mingw-w64-x86_64-ninja mingw-w64-x86_64-meson mingw-w64-x86_64-python \
  mingw-w64-x86_64-pkgconf mingw-w64-x86_64-zlib mingw-w64-x86_64-libslirp \
  mingw-w64-x86_64-libgcrypt git make diffutils
```

### The three problems, in short

1. **Symlinks abort configure.** QEMU's `symlink-install-tree.py` uses `os.symlink`, which Windows refuses without Developer Mode. `apply.sh` patches it to copy instead.
2. **slirp drags in a second, static glib.** The fork's `meson.build` asks for slirp with `static: true` and MSYS2 ships it static only, so glib is linked twice (hundreds of `multiple definition` errors), and `--disable-slirp` does not work because the block is not guarded. `apply.sh` gates the block.
3. **A build directory can be locked** by a shell that has it open. `build.sh --clean` moves it aside.

The full text, with the reasons, is in `qemu/README.md`.

## How to build and test it

Nobody should follow this as instructions yet; it is the expected shape.

1. MSYS2 MINGW64 shell, packages above.
2. `scripts/fetch-qemu-src.sh` then `qemu/build.sh` in the repository folder.
3. `cargo build --release -p shell` from a Windows shell with Rust and the MSVC C++ build tools (the earlier README named them; the current build has not been tried).
4. The result of step 2 has to be checked for which DLLs it needs from MSYS2 (`ldd`), and those would have to travel with it.

## What goes wrong

Known, from the notes: the three problems above. Everything else is unknown.

## What is not finished

- **No distributable Windows QEMU exists.**
- The build has not been re-run on Windows since the macOS-related edits to `apply.sh` and `build.sh` (the `strchrnul` edit does nothing on Windows; the rest are untested there).
- Packaging: the MSYS2 DLLs, and the licences that go with them ([chapter 17](17-licences.md)).
- `Headless` in the SDK uses POSIX process groups to stop QEMU; a Windows equivalent is not written.
- Whether the program builds, opens its window, and talks to the QEMU on Windows today.
