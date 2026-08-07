//! Patch a flash image so it boots without a radio.
//!
//! Usage: bypass-radio <flash.bin> <app.elf> <out.bin>
//!
//! The ESP32's Wi-Fi PHY is a closed blob that calibrates analog circuitry,
//! and the MAC below it is undocumented. Emulating the registers gets the
//! blob part-way and then it waits forever. This replaces the entry points
//! instead, so firmware that would otherwise stop at phy_init carries on.
//!
//! Firmware patched this way is not running what it would run on hardware.
//! That is the point, and it should never be a surprise -- so this prints
//! every patch it makes.

use flashimg::patch::{Patch, Patcher, Stub};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let flash_path = args.next().ok_or("usage: bypass-radio <flash.bin> <app.elf> <out.bin>")?;
    let elf_path = args.next().ok_or("missing <app.elf>")?;
    let out_path = args.next().ok_or("missing <out.bin>")?;

    let mut flash = std::fs::read(&flash_path)?;
    let elf = std::fs::read(&elf_path)?;
    let symbols = flashimg::ElfSymbols::parse(&elf)?;

    // Where the application image starts. Every default partition layout puts
    // it here; a board with its own layout would need this read from the
    // partition table instead.
    const APP_OFFSET: usize = 0x10000;

    // ESP_OK for everything that only sets driver state, so firmware believes
    // Wi-Fi came up; a real failure for the calls that would need a radio.
    let ok = |name: &str, reason| Patch {
        symbol: name.into(),
        stub: Stub::ReturnConst(0),
        reason,
    };
    let patches = [
        Patch {
            symbol: "esp_phy_enable".into(),
            stub: Stub::ReturnVoid,
            reason: "calibrates a radio that does not exist; never returns",
        },
        ok("esp_wifi_init", "would start the MAC"),
        ok("esp_wifi_set_mode", "driver state only"),
        ok("esp_wifi_set_config", "driver state only"),
        ok("esp_wifi_start", "would bring the MAC up"),
        ok("esp_wifi_stop", "nothing to stop"),
        ok("esp_wifi_disconnect", "nothing to disconnect"),
        ok("esp_wifi_connect", "no radio to associate with"),
        // Scanning needs beacons off the air. Failing cleanly is honest and
        // stops the caller before it reads an uninitialised result buffer;
        // returning ESP_OK without filling one hands it garbage SSIDs.
        Patch {
            symbol: "esp_wifi_scan_start".into(),
            stub: Stub::ReturnConst(1),
            reason: "no radio to hear beacons; reports failure",
        },
    ];

    let patcher = Patcher::new(&symbols, &flash, APP_OFFSET)?;
    let applied = patcher.apply(&mut flash, &patches)?;

    for (a, p) in applied.iter().zip(patches.iter()) {
        println!(
            "{:<24} {:#010x} -> flash +{:#08x}  ({} bytes)  {}",
            a.symbol, a.address, a.offset, a.bytes, p.reason
        );
    }

    std::fs::write(&out_path, &flash)?;
    println!("wrote {out_path}");
    Ok(())
}
