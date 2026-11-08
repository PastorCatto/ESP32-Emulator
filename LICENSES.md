# Licences

The licence is per item. This is the table; what was read and what was not is in
[docs/17-licences.md](docs/17-licences.md). Licence texts sit in this folder and in
[third-party/](third-party/). The written offer of source and relinking is [SOURCE.txt](SOURCE.txt).

Owner's decisions (relayed 2026-11-08): the emulator program follows QEMU's licence (GPL-2.0-or-later);
the crates that never touch QEMU code stay permissive, one licence per crate; the libraries in the
macOS QEMU build keep their own licences, texts and notices.

## This repository

| Item | Licence | Text |
|---|---|---|
| `crates/shell` (the window program `esp32-emulator`, `headless*` examples) | GPL-2.0-or-later | [LICENSE](LICENSE) |
| `crates/flashimg`, `crates/vpb`, `crates/boards`, `crates/devices`, `crates/qemuctl` | MIT OR Apache-2.0 (each crate's `Cargo.toml`) | [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE) |
| Device models added to QEMU, `qemu/devices/` (16 files) | GPL-2.0-or-later (SPDX line in each file) | [LICENSE](LICENSE) |
| `qemu/apply.sh`, `qemu/build.sh`, `scripts/`, `tools/`, `docs/` | GPL-2.0-or-later, as the repository; no per-file notice | [LICENSE](LICENSE) |
| Board files `boards/*.toml` | Facts (pin numbers, addresses) from the makers' public material; no source code copied | |
| Screenshots `docs/images/`, `screen.png` | The owner's, of PURR OS and a T-Deck. Whether any shows third-party artwork: **to be checked** | |

`shell` depends on all the permissive crates; none of them depends on `shell`. The permissive crates
depend only on each other and on Rust crates (see below).

## Not in this repository, but in a built emulator

| Item | Version | Licence | Text and notice |
|---|---|---|---|
| QEMU, Espressif fork (tag `esp-develop-9.2.2-20260417`) | 9.2.2 | GPL-2.0 as a whole; some files carry other licences in the file | [third-party/qemu/LICENSE](third-party/qemu/LICENSE), [LICENSE](LICENSE) is the GPL text |
| glib | 2.88.3 | LGPL-2.1-or-later (parts under others: MIT, Apache-2.0, MPL-1.1 and more; not mapped file by file) | [third-party/glib/](third-party/glib/) |
| libintl (gettext-runtime/intl) | gettext 1.0 | LGPL-2.1-or-later | [third-party/libintl/](third-party/libintl/) |
| libgcrypt | 1.12.4 | LGPL-2.1-or-later (some assembly files under BSD-3-Clause and others, see its `LICENSES`) | [third-party/libgcrypt/](third-party/libgcrypt/) |
| libgpg-error | 1.61 | LGPL-2.1-or-later | [third-party/libgpg-error/](third-party/libgpg-error/) |
| pcre2 | 10.48 | BSD-3-Clause WITH PCRE2-exception | [third-party/pcre2/](third-party/pcre2/) |
| libffi | 3.4.8 | MIT | [third-party/libffi/](third-party/libffi/) |
| pixman | 0.46.0 | MIT | [third-party/pixman/](third-party/pixman/) |
| libslirp | 4.9.1 | BSD-3-Clause (some files MIT) | [third-party/libslirp/](third-party/libslirp/) |
| Espressif ROM images (`esp32s3_rev0_rom.bin` and the other `.bin` in QEMU's `pc-bios`) | with the QEMU tag above | Copyright Espressif Systems (Shanghai) Co. Ltd.; **licence of the `.bin` not confirmed**; Apache-2.0 is stated for the ROM ELF files in another repository; contains third-party code (list in the notice) | [third-party/espressif-rom/NOTICE.md](third-party/espressif-rom/NOTICE.md) |
| zlib, bzip2, iconv, libSystem | from the macOS | Apple's; not distributed by us | |

For the LGPL libraries: they are linked statically into the one QEMU program. The source of each, the
build script that produced the archives, and the steps to relink are in [SOURCE.txt](SOURCE.txt).

## Rust dependencies

From `Cargo.lock` (329 packages across all platforms), by their declared licences: mostly MIT and
Apache-2.0, alone or together; also Unicode-3.0, BSD, ISC, Zlib, CC0, 0BSD, BSL-1.0 and others. Ten are
Apache-2.0 only (including `winit` and `glutin`, used by the window program); one set of default fonts
reports `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`. The licence files inside each crate were
not read for this repository. **To be checked** before shipping a binary of `shell`: the licence texts
for these have to travel with it (the kit builds `RUST-LICENSES.txt` for this).

Anything marked "to be checked" has not been read in a source tree or confirmed with its owner.
