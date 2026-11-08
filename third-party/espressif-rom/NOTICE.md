# Espressif ROM images

QEMU for the ESP32-S3 needs a copy of the chip's boot ROM. Espressif's QEMU fork ships it as
`pc-bios/esp32s3_rev0_rom.bin` (the ESP32-C3 and original ESP32 ROMs sit beside it). This
repository does **not** contain any of them. The SDK kit that carries a built QEMU does.

## What was read (2026-11-08)

| Fact | Source |
|---|---|
| File used by the S3 machine | `hw/xtensa/esp32s3.c` in the fork: `qemu_find_file(QEMU_FILE_TYPE_BIOS, "esp32s3_rev0_rom.bin")` |
| Where it comes from | `pc-bios/esp32s3_rev0_rom.bin` in https://github.com/espressif/qemu at tag `esp-develop-9.2.2-20260417` (commit `40edccac415693c5130f91c01d84176ae6008566`); installed as `share/qemu/esp32s3_rev0_rom.bin` |
| SHA-256 of that file | `b5c7090cc22efce51c1323cef31ca96fcd673a5b1d1b1017e219d393f2659913` |
| Copyright holder | "The ROMs of ESP32 series of chips are Copyright (c) 2015-2022 Espressif Systems (Shanghai) Co. Ltd." (README of https://github.com/espressif/esp-rom-elfs) |
| Third-party code inside | "The ROMs also include various third-party libraries"; the list is read, see "Third-party code in Espressif's ROMs" below |
| Source | "The ROM code of the currently available ESP32 series of chips is not open-source." (same README) |
| Licence of the **ELF** files | Apache License 2.0: "You can use the ELF files in this repository under the terms of Apache License 2.0", applying to the compiled files too (same README). Text: [LICENSE-Apache-2.0](LICENSE-Apache-2.0), byte-identical to `LICENSE` of that repository (git blob 261eeb9e9f8b2b4b0d119366dda99c6fd7d35c64) |

## What could not be confirmed

- **The licence of the `.bin` files in the QEMU fork.** The fork's `LICENSE` (copied to [../qemu/LICENSE](../qemu/LICENSE)) says the firmware files "are separate programs … and they have separate licenses", and its `pc-bios/README` does not mention any Espressif file. GitHub's licence API reports the fork's licence as "other". No text was found that places `esp32s3_rev0_rom.bin` under a licence.
- The Apache-2.0 statement above is made for the ROM **ELF** files in another repository. It is likely that the same ROM is meant, but no source says so for the QEMU `.bin`. Nobody here is entitled to say it does.
- Which of the third-party items below are in the **S3** ROM specifically. The ESP-IDF page speaks of "Espressif SoCs mask ROM" in general, not per chip.

## Third-party code in Espressif's ROMs

Read on 2026-11-08 from the "ROM Source Code Copyrights" section of the ESP-IDF page linked below. It says Espressif SoC mask ROM "includes binaries compiled from portions of" these:

| Item | Copyright and licence as the page states it |
|---|---|
| Newlib | BSD licence, copyright of various parties (ESP-IDF `COPYING.NEWLIB`) |
| Xtensa libhal | (C) 2003, 2006, 2010 Tensilica Inc, MIT (text reproduced on the page) |
| TinyBasic Plus | (C) 2012-2013 Mike Field & Scott Lawrence, MIT (text on the page) |
| miniz | Rich Geldreich, public domain |
| TJpgDec | (C) 2011 ChaN, "all right reserved", own licence text on the page |
| Zephyr USB stack: DesignWare USB device driver | (C) 2016 Intel Corporation, Apache-2.0 |
| Zephyr USB stack: generic USB device driver | (C) 2006 Bertrik Sikken, 2016 Intel Corporation, BSD-3-Clause |
| Zephyr USB stack: USB descriptors | (C) 2017 PHYTEC Messtechnik GmbH, 2017-2018 Intel Corporation, Apache-2.0 |
| Zephyr USB stack: USB DFU class driver | (C) 2015-2016 Intel Corporation, 2017 PHYTEC Messtechnik GmbH, BSD-3-Clause |
| Zephyr USB stack: USB CDC ACM class driver | (C) 2015-2016 Intel Corporation, Apache-2.0 |

The MIT, TinyBasic and TJpgDec texts are on the ESP-IDF page itself and are not copied here; the page is the source. The same page says original ESP-IDF code is "Copyright (C) 2015-2023 Espressif Systems", Apache-2.0.

## Links (all retrieved 2026-11-08 except where noted)

- ROM ELF files, copyright and Apache-2.0 statement: https://github.com/espressif/esp-rom-elfs (README, and `LICENSE`)
- QEMU fork, licence statement: https://github.com/espressif/qemu/blob/esp-develop/LICENSE (copied at the tag we use to [../qemu/LICENSE](../qemu/LICENSE))
- QEMU fork, firmware folder: https://github.com/espressif/qemu/tree/esp-develop-9.2.2-20260417/pc-bios (its `README` there does not mention the Espressif files; read in our checkout)
- QEMU fork, release with the source tarball: https://github.com/espressif/qemu/releases/tag/esp-develop-9.2.2-20260417
- ESP-IDF "Copyrights and Licenses", ROM section: https://docs.espressif.com/projects/esp-idf/en/latest/esp32/COPYRIGHT.html#rom-source-code-copyrights
- Newlib's licence file in ESP-IDF: https://github.com/espressif/esp-idf/blob/0e6b480f/components/esp_libc/COPYING.NEWLIB (link taken from the page; **not opened**)

Until Espressif says so in writing, treat the `.bin` files as: copyright Espressif Systems (Shanghai) Co. Ltd., redistributed unmodified by Espressif inside their QEMU release, licence not stated for this file.
