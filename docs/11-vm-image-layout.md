# 11. The VM image layout (flash map v3)

## Short version

- A VM image is a **whole-flash image**: 16 MB, written as if from address 0x0 of a T-Deck Plus. The emulator boots it the way the chip would: the ROM runs the bootloader at 0x0, which reads the partition table at 0xA000.
- The layout is PURR OS's **flash map v3** (2026-11-19; v2 was release 1.0.29, and differs only at the end: see the table). The authority is PURR OS's own file, [PurrOS/partitions/tdeck_plus.csv](https://github.com/PastorCatto/PURR-OS-ESP32/blob/PURR2.0/PurrOS/partitions/tdeck_plus.csv), and its chapter [docs/02-flash-map-and-partitions.md](https://github.com/PastorCatto/PURR-OS-ESP32/blob/PURR2.0/docs/02-flash-map-and-partitions.md) (repository `PastorCatto/PURR-OS-ESP32`, branch `PURR2.0`). If this page and those disagree, they are right.
- The three slots: `kittenos` at 0x20000, `kernel` at 0x1E0000, `loader` at 0x400000. The root LittleFS (`root`) follows them, from 0x5C1000, and the raw `drivers` partition is the last 0x14000 bytes of the flash.
- The emulator does not require this layout. It reads any valid table at 0x8000 or 0xA000. `flashimg::purr` can build the v3 table, tell v3 from v2 (`PartitionTable::purr_flash_map`), and fill `drivers`; a unit test pins the built table to PURR OS's golden `partition-table-tdeck_plus.bin` (a copy is in `crates/flashimg/testdata/`).
- **Untested here:** booting a flash map v2 image. The images booted in the emulator so far were 1.0.27 (the old layout).

## What it is for

The emulator needs to know nothing about PURR OS to boot it, but a VM image has to match the table the OS was built against, because the OS reads the table at an offset compiled into it and looks for `root` at a compiled-in address. An image whose table does not match boots to a failed mount or a boot loop. This page says what the layout is, what the emulator does with an image, and what to check before blaming the emulator.

## The files

| File | What it is |
|---|---|
| [crates/flashimg/src/partition.rs](../crates/flashimg/src/partition.rs) | Parsing and checking a partition table. Finds it at 0x8000 or 0xA000 |
| [crates/flashimg/src/layout.rs](../crates/flashimg/src/layout.rs) | Recognising a dropped file: merged flash image, bare app, bootloader, ELF |
| [crates/flashimg/src/provision.rs](../crates/flashimg/src/provision.rs) | Formatting an erased filesystem partition so it mounts |
| [crates/flashimg/src/littlefs.rs](../crates/flashimg/src/littlefs.rs) | The empty LittleFS volume that provisioning writes |
| [boards/t-deck-plus-vm.toml](../boards/t-deck-plus-vm.toml) | The board the VM image runs on: 16 MB flash, 8 MB octal PSRAM |
| PURR OS: `PurrOS/partitions/tdeck_plus.csv`, `docs/02-flash-map-and-partitions.md`, `plan1.md` ("Flash map v2"), `sdk/purrsdk/flash.py` (`layout_note()`) | The layout and the check |

## How it works

### The map

Quoted from PURR OS's chapter 02 (the version on branch `PURR2.0` when this page was written). Addresses are flash offsets; "End" is the first byte after the partition.

| Name | Type, subtype | Offset | Size | End | What is in it |
|---|---|---|---|---|---|
| bootloader | (not a partition) | 0x0 | 40 KB | 0xA000 | The PURR bootloader |
| partition table | | 0xA000 | 4 KB | 0xB000 | The table itself |
| `nvs` | data, nvs | 0xB000 | 16 KB | 0xF000 | ESP-IDF key/value store |
| `purrcfg` | data, 0x40 | 0xF000 | 8 KB | 0x11000 | Two copies of the boot configuration |
| `phy_init` | data, phy | 0x11000 | 4 KB | 0x12000 | Radio calibration data |
| `bootpkg` | data, 0x40 | 0x12000 | 56 KB | 0x20000 | The signed boot package |
| `kittenos` | app, factory | 0x20000 | 0x1C0000 (1.75 MB) | 0x1E0000 | KittenOS (recovery profile) |
| `kernel` | app, ota_0 | 0x1E0000 | 0x220000 (2.125 MB) | 0x400000 | The full OS image |
| `loader` | app, test | 0x400000 | 0x1C0000 (1.75 MB) | 0x5C0000 | The recovery loader |
| `netrec` | data, 0x41 | 0x5C0000 | 4 KB | 0x5C1000 | Last Wi-Fi network, for recovery |
| `root` | data, 0x83 | 0x5C1000 | 0xA2B000 (about 10.2 MiB; v2: 0xA3F000) | 0xFEC000 | LittleFS root filesystem |
| `drivers` | data, 0x42 | 0xFEC000 | 0x14000 (80 KB) | 0x1000000 | Ten 8 KB slots of signed driver modules, raw (PURR OS `Drivers/SPEC.md` section 4a). The 8 MB waveshare154 board has none |

The map fills the 16 MB exactly. PURR OS's chapter says the flash map is **designed and written into the tables and tools, and has not yet run on a device**.

### What the emulator does with an image

1. `Dropped::identify` recognises a whole-flash image by finding a partition table at 0x8000 or 0xA000 and a bootloader at 0x0 or 0x1000. A bare application image is refused (it needs a bootloader and a table); an ELF is refused.
2. The flash is sized to the board's `flash_size` (16 MB for the T-Deck boards), or the next power of two above the image if the image is bigger.
3. **Provisioning.** If a filesystem partition (data subtype 0x82 or 0x83) is entirely erased, the emulator formats it as an empty LittleFS volume. An image that already carries a filesystem is not touched. For flash map v3 this means a blank `root` becomes an empty volume of 0xA2B000 bytes (the block count comes from the table, so nothing is hard-coded), written with the emulator's own LittleFS parameters. `drivers` is raw (subtype 0x42) and is never formatted or touched by provisioning. Whether PURR OS accepts a volume formatted that way has **not been tested**.
4. The result is written to the run folder as `flash.bin` and QEMU boots that copy.
5. The emulator's notion of the "default app" (`factory`, else the lowest OTA slot) is for display only. The PURR bootloader picks its slot itself.

### Putting the drivers in

`drivers` holds the signed driver modules the kernel no longer carries, so an image without it boots a kernel with no display, keyboard or touch driver. Fill it from `drivers-<board>.bin` (made by PURR OS's `tools/release/make_drivers_partition.py` or the SDK):

- In the window: add the file as an extra image at offset 0xFEC000.
- From the command line: `cargo run -p flashimg --example place-drivers flash.bin drivers-tdeck_plus.bin out.bin` (finds the partition through the image's table, checks each slot header is blank or `PDRV`, formats a blank `root`).
- In code: `flashimg::purr::place_drivers(&mut flash, &drivers)`.

Signatures and the per-slot crc are not checked here; the device does that when it loads a driver.

### Writing the image

PURR OS builds the whole-flash image with its own tools (`tools/hw/make_hw_image.sh` for a device, `tools/vm/mkimage_dbg.sh` for the VM test image; see its chapter 02, "Building a whole-flash image"). Those scripts need the boot key's passphrase for the signed `purrcfg`, which only the owner has, so a VM image cannot be produced from this repository alone. The PURR SDK's `layout_note()` warns when an image written at 0x0 has a table that is not the current flash map.

## How to build and test it

To look at an image's table without booting it:

```sh
dd if=my-flash.bin of=table.bin bs=4096 skip=10 count=1
python3 $IDF_PATH/components/partition_table/gen_esp32part.py table.bin
```

(0xA000 is block 10 of 4096 bytes. `gen_esp32part.py` is ESP-IDF's, not part of this repository, and this command line has **not been run** here.) The unit tests for the table and for provisioning are in `crates/flashimg` and run with `cargo test -p flashimg`.

To boot: see [chapter 10](10-headless-and-trace.md), or the window with `target/release/esp32-emulator my-flash.bin boards/t-deck-plus-vm.toml --run`.

## What goes wrong

| Symptom | Cause | What to do |
|---|---|---|
| "no bootloader" or an image refused on drop | A bare app image, or a file that is not a whole-flash image | Pass the whole 16 MB image |
| Boot loop, or `root` will not mount | The table in the image is not the one the OS was built against (an older layout) | Rebuild the whole image; PURR OS says moving layouts needs the chip erased and `root` reformatted |
| The image is shorter than 16 MB | A partial image | The emulator sizes the flash to the board (or the next power of two above the image). A short image still needs its table and slots at the right addresses. How it pads has not been checked |
| A guest panic after a tap | Not a layout problem | See [chapter 10](10-headless-and-trace.md), the table of what goes wrong |

## What is not finished

- **Untested:** a flash map v3 image booting in the emulator. Only 1.0.27 images have been booted. The v3 code is unit-tested, and `place-drivers` was run on a synthetic image, not a real one.
- **Untested:** that the emulator's empty LittleFS is accepted by PURR OS's mount code.
- The window does not yet show or warn about the flash map version; `PartitionTable::purr_flash_map` is there for it.
- The doc quotes a table that PURR OS may change. Re-read the PURR chapter before relying on a number here.
