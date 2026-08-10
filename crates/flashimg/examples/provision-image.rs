//! Assemble a firmware the way the emulator does, provision its filesystem,
//! and write the result out — so the image can be checked against a real
//! littlefs without the emulator in the way.
//!
//! Usage: provision-image <firmware.bin> <flash-size-bytes> <out.bin>

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, size, out] = &args[..] else {
        eprintln!("usage: provision-image <firmware.bin> <flash-size> <out.bin>");
        std::process::exit(2);
    };

    let raw = std::fs::read(input)?;
    let size: u32 = size.parse()?;
    let dropped = flashimg::Dropped::identify(&raw);
    let image = flashimg::FlashImage::assemble(&dropped, &raw, flashimg::FlashSize(size), None)?;

    let mut bytes = image.as_bytes().to_vec();
    for action in flashimg::provision::prepare(&mut bytes) {
        println!("{action}");
    }

    let table = flashimg::PartitionTable::parse(&bytes[flashimg::TABLE_OFFSET as usize..])?;
    for p in &table.entries {
        println!(
            "  {:12} {:>9} off={:#08x} size={:#08x}",
            p.label,
            p.subtype_name(),
            p.offset,
            p.size
        );
    }

    std::fs::write(out, &bytes)?;
    println!("wrote {} ({} bytes)", out, bytes.len());
    Ok(())
}
