fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: parts <flash.bin>")?;
    let raw = std::fs::read(&path)?;
    let at = flashimg::TABLE_OFFSET as usize;
    let table = flashimg::PartitionTable::parse(&raw[at..at + flashimg::TABLE_MAX_LEN])?;
    for p in &table.entries {
        println!(
            "  {:<10} type={:?} subtype=0x{:02x} off=0x{:06x} size=0x{:06x}",
            p.label, p.ty, p.subtype, p.offset, p.size
        );
    }
    println!("  {} entries", table.entries.len());
    Ok(())
}
