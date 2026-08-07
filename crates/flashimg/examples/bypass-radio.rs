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

use flashimg::patch::Patcher;

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

    let patches = flashimg::patch::radio_bypass();

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
