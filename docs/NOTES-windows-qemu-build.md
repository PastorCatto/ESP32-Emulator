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

## Second run: slirp (user-mode networking), 2026-11-11, branch `windows-qemu-slirp`

The first build had slirp off. The VM board's Ethernet (the W5500 on a socket bridge) sits on slirp, so for `purr run` the Windows QEMU needs it.

- **MSYS2's MINGW64 environment has no libslirp package** (`pacman -Ss libslirp` finds only UCRT64, CLANG64 and CLANGARM64). The package list in the first run's commands (and in `docs/16-building-on-windows.md`) was already missing it; that was right to drop.
- **UCRT64's libslirp ships `libslirp-0.dll`, `libslirp.dll.a` and `libslirp.a`.** The fork's `meson.build` hard-codes `static: true` for slirp, which is what pulled in the second static glib. `apply.sh` now asks for the shared library on Windows (`static: host_os != 'windows'`; macOS keeps its static one). `build.sh` turns slirp on when `pkg-config --exists slirp` succeeds and leaves it off otherwise, so a MINGW64 shell still builds.
- Commands: `pacman -S --needed mingw-w64-ucrt-x86_64-{gcc,glib2,pixman,ninja,meson,python,pkgconf,zlib,libslirp,libgcrypt}` (the first attempt failed with a DNS timeout on one download; the retry worked), then in a **UCRT64** shell (`MSYSTEM=UCRT64`): `qemu/build.sh --clean`. configure said `Run-time dependency slirp found: YES 4.9.7`, `slirp support: YES`; 1,455 steps; no duplicate-definition errors. Clock stayed 3.7 to 4.1 GHz, no throttling.
- Result: `qemu-system-xtensa.exe` 63 MB and **17 DLLs** (the 16 from before, now UCRT64 builds, plus `libslirp-0.dll`). `ldd` shows nothing outside `/ucrt64/bin` and Windows.
- Checked: `-netdev help` lists `user`; `info usernet` on a `-netdev user,...,hostfwd=tcp:127.0.0.1:47811-10.0.2.15:80` shows `TCP[HOST_FORWARD] 127.0.0.1 47811 -> 10.0.2.15 80` and the host port 47811 listens; the ROM boots from an extracted zip with only `C:\Windows` on the PATH; `cargo test -p qemuctl` 33 pass; a PURR image built from source boots to the recovery loader through the SDK (26 s).
- **Not checked:** a guest that gets an address and reaches `10.0.2.2` (needs a VM-flavour image).
- One more mistake: I called `info network` over stdio at first and forgot `-L`, so QEMU stopped at "ROM code binary not found"; with `-L ..\share\qemu` it ran. A later `info network` typed through stdio came back garbled by the console echo; the monitor over TCP (`-monitor tcp:127.0.0.1:PORT,server,nowait`) is cleaner for scripts.