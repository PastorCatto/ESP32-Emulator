//! Format a disk image as FAT16 so a guest can actually mount it.
//!
//! Usage: format-sd <card.img> [LABEL]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: format-sd <card.img> [LABEL]")?;
    let label = args.next().unwrap_or_else(|| "ESP32EMU".into());

    let mut image = std::fs::read(&path)?;
    let layout = flashimg::fat::format_in_place(&mut image, &label)?;
    std::fs::write(&path, &image)?;

    println!(
        "{path}: FAT16, {} sectors, {} bytes/cluster, {} clusters, {} FAT sectors",
        layout.total_sectors,
        layout.sectors_per_cluster * flashimg::fat::SECTOR,
        layout.clusters,
        layout.fat_sectors
    );
    Ok(())
}
