//! Put a drivers image into the `drivers` partition of a whole flash image
//! (PURR OS flash map v3), format a blank `root`, and write the result.
//!
//! Usage: place-drivers <flash.bin> <drivers-<board>.bin> <out.bin>
//!
//! The partition is found through the image's own table. The same thing can be
//! done in the window by adding the drivers file as an extra image at 0xFEC000.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, flash_path, drivers_path, out] = &args[..] else {
        eprintln!("usage: place-drivers <flash.bin> <drivers.bin> <out.bin>");
        std::process::exit(2);
    };

    let mut flash = std::fs::read(flash_path)?;
    let drivers = std::fs::read(drivers_path)?;

    if let Some(at) = flashimg::PartitionTable::locate(&flash) {
        let table = flashimg::PartitionTable::parse(&flash[at as usize..])?;
        println!("{}", table.purr_flash_map());
    }
    println!("{}", flashimg::purr::place_drivers(&mut flash, &drivers)?);
    for action in flashimg::provision::prepare(&mut flash) {
        println!("{action}");
    }

    std::fs::write(out, &flash)?;
    println!("wrote {out} ({} bytes)", flash.len());
    Ok(())
}
