# Testrun

A known-good set of files for exercising the emulator by hand. Drop all three
onto the window, press **Bypass + boot**, and PURR OS reaches its home screen
in about 45 seconds.

| File | What it is |
| --- | --- |
| `purros-tdeck-plus.bin` | 16 MB flash image: bootloader at 0x0, partition table at 0x8000, app at 0x10000 |
| `purros-tdeck-plus.elf` | Symbols for **that exact build**. Patching locates functions by symbol, so this is required, not optional |
| `sdcard.img` | 64 MB blank card. The card initialises; FATFS then reports `FR_NO_FILESYSTEM`, because nothing has formatted it |

The `.bin` and `.elf` are a matched pair and only useful together. The ELF's
own SHA-256 is `34163ac5cf2542e9…`; a `.bin` built from a different compile of
the same source will have different addresses, the patcher will write to the
wrong places, and the result will crash somewhere unrelated. If you rebuild the
firmware, replace both.

## Where these came from

Assembled from `PURR-OS-ESP32/CoreOS/build_tdeck_plus/`, which is the one build
directory whose `.elf` matches an image we can boot. The three parts were laid
into a blank 16 MB image at the offsets above — the same layout `esptool
merge_bin` produces.

## Without the window

```sh
cargo run -p flashimg --example bypass-radio -- \
    Testrun/purros-tdeck-plus.bin Testrun/purros-tdeck-plus.elf /tmp/patched.bin
scripts/run-emu.sh /tmp/patched.bin 150 Testrun/sdcard.img
```

## What "bypass" means

The Wi-Fi PHY is a closed blob calibrating analog circuitry that does not exist
under emulation, and the MAC below it is undocumented. Unpatched, this firmware
stops inside `phy_init` and never returns.

The bypass replaces `esp_phy_enable` and the `esp_wifi_*` entry points with
stubs. **Firmware patched this way is not running the code it would run on
hardware** — the Wi-Fi it reports is not real, and the scan call reports failure
rather than returning an empty list, because an empty list is
indistinguishable from a real scan of an empty room.

Boot it unpatched to see the original behaviour: it gets as far as
`phy_init: failed to load RF calibration data ... falling back to full
calibration` and stops there.
