# Notes: building the patched QEMU for Windows (first real run)

Run on Windows 11 Home (10.0.26200), 2026-11-11, branch `windows-qemu` (from `main` at `e629628`). Everything below was run, not guessed. Nothing was pushed.

## What came out

`qemu-system-xtensa.exe` (QEMU 9.2.2, the Espressif fork at tag `esp-develop-9.2.2-20260417` with our device models), 64.7 MB, plus 16 MSYS2 DLLs, the four ROM files, and the two Rust programs `esp32-emulator.exe` and `headless_tap.exe`. The whole staging folder is 84 MB. Hashes are in `SHA256SUMS-windows.txt` (next to this build, not committed).

## What was checked

- `qemu-system-xtensa.exe --version` says 9.2.2; `-machine help` lists `esp32` and `esp32s3`.
- Smoke test with a **minimal PATH** (`C:\Windows\System32;C:\Windows` only), the DLLs beside the exe, an 8 MB blank flash, `-machine esp32s3 -m 8M`: it prints the ROM banner on the serial port (`ESP-ROM:esp32s3-20210327`, `rst:0x1 (POWERON),boot:0x4 (SPI_FLASH_BOOT)`, then `invalid header` lines for the blank flash).
- QMP over TCP (`-qmp tcp:127.0.0.1:PORT,server,nowait`) answers with its greeting.
- `cargo test -p qemuctl` with `ESP32_EMULATOR_REQUIRE_QEMU=1`: 27 + 6 tests pass (after the ROM step below).
- `cargo build --release --locked -p shell --bin esp32-emulator --example headless_tap` builds (1m38s on a cold cache). `headless_tap` starts and prints its usage.

## NOT checked

- A full run of `headless_tap` or `esp32-emulator`: they need a real flash image (a blank file is "Unrecognised") and a board file via `BOARD_TOML`. The VM image lives on the Mac.
- The Rust programs on a PC that does not have the Visual C++ build tools: they link the C runtime dynamically by default, so a clean PC may need the VC++ redistributable. Not tested.
- Whether the build is reproducible on another machine.

## The commands that worked

1. `winget install MSYS2.MSYS2` (installs to `C:\msys64`).
2. From a MINGW64 shell: `pacman -Syu --noconfirm` (twice, as usual), then
   `pacman -S --needed --noconfirm mingw-w64-x86_64-gcc mingw-w64-x86_64-glib2 mingw-w64-x86_64-pixman mingw-w64-x86_64-ninja mingw-w64-x86_64-meson mingw-w64-x86_64-python mingw-w64-x86_64-pkgconf mingw-w64-x86_64-zlib mingw-w64-x86_64-libgcrypt git make diffutils`
   (**no libslirp**, see mistake 2). Got gcc 16.2.0, meson 1.12.1, ninja 1.13.2, Python 3.14.10.
3. `scripts/fetch-qemu-src.sh`, then `qemu/build.sh --clean`. About 1,450 compile steps.
4. Copy the ROM files into `vendor/qemu/share/qemu/` (mistake 4).
5. `winget install Rustlang.Rustup` and `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`.
6. `cargo build --release --locked -p shell --bin esp32-emulator --example headless_tap`.

## Mistakes and surprises, in order

1. **Smart App Control blocked MSYS2.** On this PC Smart App Control was on (registry `VerifiedAndReputablePolicyState` = 1). Windows said "An Application Control policy has blocked this file" for `uname.exe` and `tail.exe`, and Defender folder exclusions did nothing for it. It had to be switched off (value 0). The same block also stopped the MSYS2 post-update hooks, which left a 0-byte `/usr/ssl/certs/ca-bundle.crt`, so every `pacman` download failed with "error adding trust anchors". Fix: `update-ca-trust`, then `pacman -Syy`. **Any PC with Smart App Control on will also block the finished `qemu-system-xtensa.exe`**, because it is unsigned.
2. **`mingw-w64-x86_64-libslirp` no longer exists** in the MINGW64 repository (it is only in UCRT64 and CLANG64). `docs/16-building-on-windows.md` and `qemu/README.md` still list it. The build turns slirp off on Windows anyway, so it is not needed.
3. **`apply.sh`'s symlink fix did not match its README.** The README says a missing entry is skipped; the patch re-raised every copy error. Configure died at the postconf script with `FileNotFoundError` copying `build/subprojects/dtc/libfdt/libfdt.a` (it does not exist at configure time). Fix (this branch): catch `FileNotFoundError` in the copy fallback and skip it. The already-patched script had to be restored first (`git checkout scripts/symlink-install-tree.py` in `vendor/src`) because `apply.sh` skips files that already contain `import shutil`.
4. **`build.sh` installs only the exe, not the ROM files.** `cargo test -p qemuctl` failed two boot tests (no serial output, QMP connect timed out) because QEMU exited at once: no `esp32s3_rev0_rom.bin` under `vendor/qemu/share/qemu`. The ROMs are in `vendor/src/pc-bios/`. Copying them fixed it.
5. **The first MSVC Build Tools install exited with 1602** (cancelled; the elevation prompt was not approved). A second run worked.
6. `esp32-emulator.exe --help` opens the window instead of printing help.

## The DLLs (from `ldd`, with the MSYS2 package, version and licence)

| DLL | Package | Version | Licence |
|---|---|---|---|
| libbz2-1.dll | mingw-w64-x86_64-bzip2 | 1.0.8-4 | custom (bzip2) |
| libffi-8.dll | mingw-w64-x86_64-libffi | 3.8.0-1 | MIT |
| libgcrypt-20.dll | mingw-w64-x86_64-libgcrypt | 1.12.4-2 | LGPL-2.1-or-later AND GPL-2.0-or-later |
| libgio-2.0-0.dll, libglib-2.0-0.dll, libgmodule-2.0-0.dll, libgobject-2.0-0.dll | mingw-w64-x86_64-glib2 | 2.90.1-1 | LGPL-2.1-or-later |
| libgpg-error-0.dll | mingw-w64-x86_64-libgpg-error | 1.61-1 | LGPL-2.1-or-later |
| libiconv-2.dll | mingw-w64-x86_64-libiconv | 1.19.1-1 | LGPL-2.1-or-later (docs GPL-3.0-or-later) |
| libintl-8.dll | mingw-w64-x86_64-gettext-runtime | 1.0-1 | GPL-3.0-or-later AND LGPL-2.1-or-later |
| libpcre2-8-0.dll | mingw-w64-x86_64-pcre2 | 10.49-2 | BSD-3-Clause |
| libpixman-1-0.dll | mingw-w64-x86_64-pixman | 0.46.4-3 | MIT |
| libwinpthread-1.dll | mingw-w64-x86_64-libwinpthread | 14.0.0.r598.ge3d18511f-1 | MIT AND BSD-3-Clause |
| libz-1.dll, zlib1.dll | mingw-w64-x86_64-zlib | 1.4-2 | Zlib |
| libzstd.dll | mingw-w64-x86_64-zstd | 1.5.7-2 | BSD-3-Clause OR GPL-2.0-or-later |

The licence strings are what `pacman -Qi` reports, not a legal reading. The GPL source offer for QEMU has to cover this build too; GPL/LGPL components here (libgcrypt, libintl, libiconv, glib) also need their source offered, and `libgcrypt` is only needed because the fork's `esp32_flash_enc.c` includes `gcrypt.h`.

## For the doc page

`docs/16-building-on-windows.md` should say: the package list without libslirp; Smart App Control must be off; `build.sh` does not install the ROMs; and the build completes today.
