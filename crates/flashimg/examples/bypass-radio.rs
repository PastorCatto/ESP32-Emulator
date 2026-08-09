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
    let flash_path =
        args.next().ok_or("usage: bypass-radio <flash.bin> <app.elf|-> <out.bin>")?;
    let elf_path = args.next().ok_or("missing <app.elf>, or - to use byte signatures")?;
    let out_path = args.next().ok_or("missing <out.bin>")?;

    let mut flash = std::fs::read(&flash_path)?;

    // Where the application image starts. Every default partition layout puts
    // it here; a board with its own layout would need this read from the
    // partition table instead.
    const APP_OFFSET: usize = 0x10000;

    // `-` for firmware whose .elf we do not have, which is most of what
    // arrives as a prebuilt binary.
    let by_signature = elf_path == "-";
    let elf = if by_signature { Vec::new() } else { std::fs::read(&elf_path)? };
    let symbols = (!by_signature)
        .then(|| flashimg::ElfSymbols::parse(&elf))
        .transpose()?;

    let mut patches = flashimg::patch::radio_bypass();
    if by_signature {
        // Two of the entry points are ten-byte wrappers with no distinctive
        // shape. Dropping them is honest; matching them by guess is not.
        let before = patches.len();
        patches.retain(|p| flashimg::signatures::has(&p.symbol));
        if patches.len() != before {
            eprintln!(
                "note: {} of {before} targets have no usable signature and are left alone",
                before - patches.len()
            );
        }
    }

    let patcher = match &symbols {
        Some(s) => Patcher::new(s, &flash, APP_OFFSET)?,
        None => Patcher::from_signatures(&flash, APP_OFFSET)?,
    };
    let outcome = patcher.apply(&mut flash, &patches)?;
    let applied = &outcome.applied;

    for (a, p) in applied.iter().zip(patches.iter()) {
        println!(
            "{:<24} {:#010x} -> flash +{:#08x}  ({} bytes)  [{}]  {}",
            a.symbol,
            a.address,
            a.offset,
            a.bytes,
            match a.how {
                flashimg::patch::Located::Symbol => "symbol",
                flashimg::patch::Located::Signature => "signature",
            },
            p.reason
        );
    }

    for s in &outcome.skipped {
        eprintln!("skipped {:<24} {}", s.symbol, s.reason);
    }
    if !outcome.radio_disabled() {
        eprintln!("WARNING: esp_phy_enable was not replaced -- this will still stall in phy_init");
    }

    std::fs::write(&out_path, &flash)?;
    println!("wrote {out_path}");
    Ok(())
}
