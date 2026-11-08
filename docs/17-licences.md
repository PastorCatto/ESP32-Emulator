# 17. Licences

## Short version

- **The licence is per item.** There is no single licence for "everything in the box".
- The owner decided (relayed 2026-11-08): the emulator program and the QEMU device models **follow QEMU** (`GPL-2.0-or-later`, text in [LICENSE](../LICENSE)); the five crates that never contain QEMU code are `MIT OR Apache-2.0`, one licence per crate. The table is [LICENSES.md](../LICENSES.md).
- Before that, `Cargo.toml` said `MIT OR Apache-2.0` and no licence text existed anywhere in the repository. Nothing written down in the repository grants anything before this change.
- Each line below says what was **read** and what is **to be checked**. I am not a lawyer; where a line is a legal judgement it says so.
- Open points for the owner are listed at the end.

## What it is for

Someone who gets a built emulator, or this source, needs to know what they may do with each part, and what must travel with it. The QEMU binary is distributed in the SDK kit, so the GPL's source obligations apply to it.

## The files

| File | What it is |
|---|---|
| [LICENSE](../LICENSE) | The GPL version 2 text, copied unchanged from `COPYING` in the QEMU tree we build from |
| [LICENSES.md](../LICENSES.md) | The one-page table |
| [LICENSE-MIT](../LICENSE-MIT), [LICENSE-APACHE](../LICENSE-APACHE) | For the five permissive crates. The Apache text is the standard one (byte-identical to the copy shipped in the `serde` crate); the MIT text has the copyright line `Copyright (c) 2026 PastorCatto`, taken from the repository's git author name, **not confirmed as the legal name to use** |
| [Cargo.toml](../Cargo.toml) and `crates/*/Cargo.toml` | `GPL-2.0-or-later` in `[workspace.package]`, used by `shell`; `license = "MIT OR Apache-2.0"` in `flashimg`, `vpb`, `boards`, `devices`, `qemuctl` |
| [SOURCE.txt](../SOURCE.txt) | The written offer of QEMU's source, our changes, the library versions, and the relink steps |
| [third-party/](../third-party/) | Licence text and notices copied from the library tarballs and from Espressif (below) |
| [qemu/devices/](../qemu/devices/) | 16 source files, each with `SPDX-License-Identifier: GPL-2.0-or-later` (checked by `grep`) |
| PURR OS repository, `sdk/NOTICES-emulator.md` | The notices that go into the kit, with the written offer for QEMU's source. Written there, not here |

## How it works, item by item

### QEMU (the patched program)

What the tree we build from states (`vendor/src/LICENSE`, read in full):

1. "The QEMU emulator as a whole is released under the GNU General Public License, version 2."
2. "Parts of the QEMU emulator have specific licenses which are compatible with the GNU General Public License, version 2. Hence each source file contains its own licensing information. Source files with no licensing information are released under the GNU General Public License, version 2 or (at your option) any later version." Version 2 only is accepted for `bsd-user/`, `linux-user/`, `hw/vfio/`, `hw/xen/xen_pt*`.
3. The Tiny Code Generator is "mostly under the BSD or MIT licenses; but some parts may be GPLv2 or other licenses".
4. "QEMU is a trademark of Fabrice Bellard." and the firmware files shipped with QEMU "are separate programs … they have separate licenses".

`vendor/src/COPYING` is the GPL version 2 (header "Version 2, June 1991"); `COPYING.LIB` is the LGPL version 2.1 (read: its header only). Espressif's fork is that tree at the tag `esp-develop-9.2.2-20260417`.

Our additions are new files (the 16 under `qemu/devices/`) and small edits made to Espressif's files by `qemu/apply.sh`. Edits to a GPL file are GPL. The new files say GPL-2.0-or-later.

**The ROM images** are covered in their own section below and in [third-party/espressif-rom/NOTICE.md](../third-party/espressif-rom/NOTICE.md).

### This repository's Rust crates

- `shell` (the window program) is `GPL-2.0-or-later`.
- `flashimg`, `vpb`, `boards`, `devices` and `qemuctl` are `MIT OR Apache-2.0`, the licence the workspace had before. They talk to QEMU over a socket or a process boundary and contain no QEMU code (a search for "GPL", "ported from" and "derived from" in them found nothing; the mentions of "qemu" are names and comments; the provenance of each file was not audited line by line, so the owner should confirm none was copied from QEMU). `boards` needs `qemuctl` for one type, so `qemuctl` is in the permissive group too; this was my choice, not named by the owner. None of the five depends on `shell`.
- A programme that includes a permissive crate and `shell` as a whole is GPL; the permissive crates themselves can be reused alone.

### The static libraries in the macOS QEMU build

Versions and files **read** in the tarballs ([chapter 14](14-building-on-macos.md)):

| Library | Version | What the tarball says | Notes |
|---|---|---|---|
| glib | 2.88.3 | `COPYING` is the LGPL 2.1; `SPDX-License-Identifier: LGPL-2.1-or-later` in the files read (`glib.h`, `gstrfuncs.c`) | `LICENSES/` also holds MIT, Apache-2.0, MPL-1.1, CC0, AFL-2.0, CC-BY-SA-3.0, GPL-2.0-or-later and others for some parts. **Which files use which was not mapped. To be checked** |
| libgcrypt | 1.12.4 | `COPYING.LIB` (LGPL 2.1) and `COPYING` (GPL); header says LGPL "version 2.1 … or (at your option) any later version" | `LICENSES` lists BSD-3-Clause and other notices for some assembly files, which "require that a copy of the notice be included … with any binary distributions" |
| libgpg-error | 1.61 | `COPYING.LIB` (LGPL 2.1); header: LGPL 2.1 or later | |
| libintl (from gettext-runtime/intl) | gettext 1.0 | `COPYING.LIB` (LGPL 2.1); `dcigettext.c` header: LGPL 2.1 or later | Only `gettext-runtime/intl` was built. The rest of gettext has GPL parts that are not linked |
| pcre2 | 10.48 | `LICENCE.md`: `BSD-3-Clause WITH PCRE2-exception` | |
| libffi | 3.4.8 | `LICENSE` is the MIT text ("Permission is hereby granted") | |
| pixman | 0.46.0 | `COPYING` is the MIT text | |
| libslirp | 4.9.1 | `meson.build` says BSD-3-Clause; files carry SPDX lines of either `BSD-3-Clause` (`ip_icmp.c`) or `MIT` (`slirp.c`); `COPYRIGHT` is the old Slirp BSD text | The kit's notice says BSD-3-Clause only; some files are MIT |
| zlib, bzip2, iconv | | From the macOS SDK; not built or shipped by us | |

**Static linking.** The macOS build links the LGPL libraries into the one program statically, where the earlier build shipped them as separate files. The owner's decision: no legal judgement is made here; each library keeps its own licence, its licence text and copyright notices are shipped ([third-party/](../third-party/)), each is listed in [LICENSES.md](../LICENSES.md), and [SOURCE.txt](../SOURCE.txt) offers the source and the relink steps. Whether that satisfies the LGPL-2.1 in every detail has not been checked by anyone qualified. The kit's notice (`NOTICES-emulator.md`) describes the earlier, dynamic build ("separate files you can replace") and **has to be rewritten** for the static one. The three-year period in `SOURCE.txt` follows the usual GPL wording and was written by me; the owner should confirm it.

### The Rust dependencies

From `cargo metadata --locked`: 329 packages in `Cargo.lock` across all platforms (fewer are compiled for one computer). By declared licence, the largest groups are `MIT OR Apache-2.0` (147), `MIT` (69), `Apache-2.0 OR MIT` (27), `Unicode-3.0` (18) and `MIT/Apache-2.0` (11). Others are BSD, ISC, Zlib, Unlicense, CC0, BSL-1.0, 0BSD, and combinations of these.

Worth looking at by name:

- **Apache-2.0 only (10 packages):** `codespan-reporting`, `gethostname`, `gl_generator`, `glutin`, `glutin_egl_sys`, `glutin_glx_sys`, `glutin_wgl_sys`, `khronos_api`, `unicode-general-category`, `winit`. `glutin` and `winit` are linked into the window program. The Apache-2.0 licence is not compatible with GPL version 2 **only**, and is compatible with GPL version 3. That is a reason for "or later", and a reason the decision should be checked.
- **`Apache-2.0 OR GPL-2.0-only` (1)** and **`MIT OR Apache-2.0 OR LGPL-2.1-or-later` (2):** each offers a choice; the permissive option is the one to use.
- **Fonts:** one package reports `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`, the default fonts for the window. The licence texts for those have to travel with a binary. **To be checked.** The kit builds `RUST-LICENSES.txt` for this.
- None declares no licence.

Declared licences are what the crates say about themselves; the licence files inside each crate were not read for this page. **To be checked.**

### Board data

The files in `boards/` are numbers and names: pin assignments, bus addresses, chip IDs. They were taken from the makers' public material (for the T-Deck, LilyGO's `utilities.h`, cited in the file; the CYD 2.8" board from a published pinout (Random Nerd Tutorials, cited in the file), the other two CYD boards from PURR OS's own device files). No source code was copied. Pin numbers are facts.

### The Espressif ROM images

Full account in [third-party/espressif-rom/NOTICE.md](../third-party/espressif-rom/NOTICE.md). In short: the file the S3 machine loads is `pc-bios/esp32s3_rev0_rom.bin` in Espressif's QEMU fork (SHA-256 `b5c7090c…9913`). Espressif states the copyright of its ROMs ("Copyright (c) 2015-2022 Espressif Systems (Shanghai) Co. Ltd.") and that the ROM code is not open source, and licenses the ROM **ELF** files in `esp-rom-elfs` under Apache-2.0. **No text was found that states the licence of the `.bin` in the QEMU fork.** It is probably the same ROM, but that is a guess and is recorded as one. The ROM also contains third-party code (Newlib, Xtensa libhal, TinyBasic Plus, miniz, TJpgDec, parts of the Zephyr USB stack); the ESP-IDF page listing them was read on 2026-11-08 and is summarised in the notice, which also holds every relevant Espressif link. The page is general to Espressif SoCs, not specific to the S3.

### Screenshots

`docs/images/*.png` and the root `screen.png` are screenshots of PURR OS and of a T-Deck running it. They show the owner's work. Whether any shows third-party artwork or fonts has not been looked at. **To be checked.**

### What this repository does not hold

It holds no QEMU binary, no ROM image and no flash image: `vendor/`, `run/` and `Testrun/` are ignored ([.gitignore](../.gitignore)). QEMU's source obligations arise where a binary is handed out, which is the SDK kit. The written offer lives in the kit's notices. This repository carries our changes to QEMU in full source form under `qemu/`.

## How to build and test it

Nothing to build. To re-check the numbers above:

```sh
cargo metadata --format-version 1 --locked | python3 -c 'import json,sys,collections as c; d=json.load(sys.stdin); print(c.Counter(p.get("license") for p in d["packages"]).most_common())'
grep -rh "SPDX-License-Identifier" qemu/devices | sort | uniq -c
```

## What goes wrong

| Symptom | Cause |
|---|---|
| A crate's `Cargo.toml` says another licence than this page | It overrides the workspace field. Today none does |
| The kit notice and this page disagree | The kit notice describes the dynamic macOS build; this page the static one |
| A new source file has no licence line | Add `SPDX-License-Identifier: GPL-2.0-or-later` (as the other 16 do) |

## What is not finished, and the owner's open points

1. **GPL version.** Decided: "2.0 or later" is fine.
2. **Permissive crates.** Decided: allowed; done as above.
3. The LGPL libraries linked statically: handled as the owner decided (texts, notices, offer); not legally reviewed.
4. The kit's `NOTICES-emulator.md` has to be rewritten for the static build.
5. Screenshot content; the Espressif ROM `.bin` licence (ask Espressif, or find their statement); the copyright holder name in `LICENSE-MIT`; the three-year period in `SOURCE.txt`; the unread licence texts of the Rust dependencies.
6. `SOURCE.txt` is here now; the kit's own notice still has to agree with it.
