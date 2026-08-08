//! Formatting a blank disk image as FAT16.
//!
//! An unformatted card is not a neutral starting point. ESP-IDF's FATFS tries
//! to mount it, gets `FR_NO_FILESYSTEM`, and the board support retries -- on a
//! T-Deck that is three attempts with backoff, about 2.1 seconds, and the
//! display driver does not start until they finish. So a card nobody
//! formatted does not merely fail to appear: it delays the boot it is not
//! part of.
//!
//! FAT16 rather than FAT32 because the cards involved are small. FAT32 needs
//! at least 65 525 clusters to be legal, which a 64 MB image cannot reach at
//! any sensible cluster size, and a mis-declared FAT type is exactly the sort
//! of thing that mounts on a desktop and fails on an embedded driver.

/// A sector, in bytes. Every offset here is in these units.
pub const SECTOR: usize = 512;

/// Directory entries in the root. 512 is the conventional value and makes the
/// root exactly 32 sectors.
const ROOT_ENTRIES: usize = 512;
const RESERVED_SECTORS: usize = 1;
const FAT_COUNT: usize = 2;

/// FAT16 is only legal in this cluster range. Below it the driver must read
/// FAT12, above it FAT32; declaring the wrong one is a mount failure on
/// anything stricter than a desktop.
const MIN_FAT16_CLUSTERS: usize = 4085;
const MAX_FAT16_CLUSTERS: usize = 65524;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub total_sectors: usize,
    pub sectors_per_cluster: usize,
    pub fat_sectors: usize,
    pub root_sectors: usize,
    pub clusters: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatError {
    /// Too small to hold a FAT16 filesystem with any cluster size.
    TooSmall(usize),
    /// Too large: even the biggest cluster size overflows the cluster count.
    TooLarge(usize),
}

impl std::fmt::Display for FatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FatError::TooSmall(n) => write!(f, "{n} bytes is too small for FAT16"),
            FatError::TooLarge(n) => write!(f, "{n} bytes is too large for FAT16"),
        }
    }
}

impl std::error::Error for FatError {}

/// Work out a legal FAT16 geometry for an image of this size.
///
/// Cluster size is the only free variable, so it is raised until the cluster
/// count drops into the legal window. The FAT has to be big enough to hold an
/// entry per cluster, and growing it shrinks the data area and so the cluster
/// count -- hence the fixed point rather than a single calculation.
pub fn plan(total_bytes: usize) -> Result<Layout, FatError> {
    let total_sectors = total_bytes / SECTOR;
    let root_sectors = ROOT_ENTRIES * 32 / SECTOR;

    for spc in [1usize, 2, 4, 8, 16, 32, 64] {
        // Solve for the FAT size: each iteration shrinks the data area, which
        // shrinks the cluster count, which may shrink the FAT again.
        let mut fat_sectors = 1usize;
        for _ in 0..8 {
            let overhead = RESERVED_SECTORS + FAT_COUNT * fat_sectors + root_sectors;
            if total_sectors <= overhead {
                break;
            }
            let clusters = (total_sectors - overhead) / spc;
            let needed = ((clusters + 2) * 2).div_ceil(SECTOR);
            if needed == fat_sectors {
                break;
            }
            fat_sectors = needed;
        }

        let overhead = RESERVED_SECTORS + FAT_COUNT * fat_sectors + root_sectors;
        if total_sectors <= overhead {
            continue;
        }
        let clusters = (total_sectors - overhead) / spc;
        if (MIN_FAT16_CLUSTERS..=MAX_FAT16_CLUSTERS).contains(&clusters) {
            return Ok(Layout {
                total_sectors,
                sectors_per_cluster: spc,
                fat_sectors,
                root_sectors,
                clusters,
            });
        }
    }

    if total_sectors < MIN_FAT16_CLUSTERS {
        Err(FatError::TooSmall(total_bytes))
    } else {
        Err(FatError::TooLarge(total_bytes))
    }
}

/// The metadata at the front of a formatted card: boot sector, both FATs, and
/// an empty root directory.
///
/// The data area is left alone. A fresh image is already zeroed, and zeroed
/// clusters are what an empty filesystem expects, so there is nothing to
/// write out there -- which also means formatting a 64 MB image touches about
/// 150 KB rather than all of it.
pub fn metadata(layout: Layout, volume_label: &str) -> Vec<u8> {
    let meta_sectors =
        RESERVED_SECTORS + FAT_COUNT * layout.fat_sectors + layout.root_sectors;
    let mut out = vec![0u8; meta_sectors * SECTOR];

    // ---- boot sector / BIOS parameter block ----
    let boot = &mut out[..SECTOR];
    // A jump nobody executes, but drivers sanity-check the first byte.
    boot[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = layout.sectors_per_cluster as u8;
    boot[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
    boot[16] = FAT_COUNT as u8;
    boot[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
    // The 16-bit total is zero whenever the 32-bit one is used, and a card
    // this size always needs the 32-bit field.
    let small_total = u16::try_from(layout.total_sectors).unwrap_or(0);
    boot[19..21].copy_from_slice(&small_total.to_le_bytes());
    boot[21] = 0xf8; // fixed disk
    boot[22..24].copy_from_slice(&(layout.fat_sectors as u16).to_le_bytes());
    boot[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors per track
    boot[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    boot[28..32].copy_from_slice(&0u32.to_le_bytes()); // hidden sectors
    let big_total = if small_total == 0 { layout.total_sectors as u32 } else { 0 };
    boot[32..36].copy_from_slice(&big_total.to_le_bytes());
    boot[36] = 0x80; // drive number
    boot[38] = 0x29; // extended boot signature: the next three fields are valid
    boot[39..43].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    let mut label = *b"NO NAME    ";
    for (slot, ch) in label.iter_mut().zip(volume_label.bytes()) {
        *slot = ch.to_ascii_uppercase();
    }
    boot[43..54].copy_from_slice(&label);
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510] = 0x55;
    boot[511] = 0xaa;

    // ---- both FATs ----
    // Entry 0 repeats the media descriptor, entry 1 is the end-of-chain mark.
    // Every other entry stays zero, which means "free".
    for i in 0..FAT_COUNT {
        let start = (RESERVED_SECTORS + i * layout.fat_sectors) * SECTOR;
        out[start..start + 4].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff]);
    }

    // Root directory is left zeroed: an all-zero entry means "no more here".
    out
}

/// Format a disk image in place, writing only the metadata region.
pub fn format_in_place(image: &mut [u8], volume_label: &str) -> Result<Layout, FatError> {
    let layout = plan(image.len())?;
    let meta = metadata(layout, volume_label);
    image[..meta.len()].copy_from_slice(&meta);
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_64mb_card_lands_inside_the_fat16_cluster_window() {
        let layout = plan(64 * 1024 * 1024).expect("64 MB is a normal card");
        assert!(
            (MIN_FAT16_CLUSTERS..=MAX_FAT16_CLUSTERS).contains(&layout.clusters),
            "{} clusters is not legal FAT16",
            layout.clusters
        );
        // The FAT must actually be able to address every cluster it claims.
        let addressable = layout.fat_sectors * SECTOR / 2;
        assert!(addressable >= layout.clusters + 2, "FAT too small for its clusters");
    }

    #[test]
    fn every_ordinary_card_size_can_be_planned() {
        for mb in [8usize, 16, 32, 64, 128, 256, 512, 1024] {
            let layout = plan(mb * 1024 * 1024)
                .unwrap_or_else(|e| panic!("{mb} MB should be formattable: {e}"));
            assert!(
                (MIN_FAT16_CLUSTERS..=MAX_FAT16_CLUSTERS).contains(&layout.clusters),
                "{mb} MB gave {} clusters",
                layout.clusters
            );
        }
    }

    #[test]
    fn the_boot_sector_says_what_a_driver_checks() {
        let layout = plan(64 * 1024 * 1024).unwrap();
        let meta = metadata(layout, "EMU");

        assert_eq!(&meta[510..512], &[0x55, 0xaa], "boot signature");
        assert_eq!(u16::from_le_bytes([meta[11], meta[12]]), 512, "bytes per sector");
        assert_eq!(meta[13] as usize, layout.sectors_per_cluster);
        assert_eq!(meta[16] as usize, FAT_COUNT);
        assert_eq!(u16::from_le_bytes([meta[17], meta[18]]) as usize, ROOT_ENTRIES);
        assert_eq!(&meta[54..62], b"FAT16   ", "fs type");
        assert_eq!(&meta[43..46], b"EMU", "volume label is applied");

        // A card this size cannot fit in the 16-bit sector count, so the
        // 32-bit field carries it and the 16-bit one must read zero.
        assert_eq!(u16::from_le_bytes([meta[19], meta[20]]), 0);
        assert_eq!(
            u32::from_le_bytes([meta[32], meta[33], meta[34], meta[35]]) as usize,
            layout.total_sectors
        );
    }

    #[test]
    fn both_fats_start_with_the_reserved_entries() {
        let layout = plan(64 * 1024 * 1024).unwrap();
        let meta = metadata(layout, "EMU");
        for i in 0..FAT_COUNT {
            let at = (RESERVED_SECTORS + i * layout.fat_sectors) * SECTOR;
            assert_eq!(&meta[at..at + 4], &[0xf8, 0xff, 0xff, 0xff], "FAT {i}");
        }
    }

    #[test]
    fn the_root_directory_is_empty() {
        let layout = plan(64 * 1024 * 1024).unwrap();
        let meta = metadata(layout, "EMU");
        let root = (RESERVED_SECTORS + FAT_COUNT * layout.fat_sectors) * SECTOR;
        assert!(meta[root..].iter().all(|&b| b == 0), "root should be all zeros");
    }

    #[test]
    fn formatting_touches_only_the_front_of_the_image() {
        let mut image = vec![0xaau8; 64 * 1024 * 1024];
        let layout = format_in_place(&mut image, "EMU").unwrap();
        let meta_len =
            (RESERVED_SECTORS + FAT_COUNT * layout.fat_sectors + layout.root_sectors) * SECTOR;
        // The data area is deliberately left as it was -- formatting a card
        // does not scrub it, and rewriting 64 MB to change 150 KB would be
        // slow for no benefit.
        assert!(image[meta_len..].iter().all(|&b| b == 0xaa));
    }

    #[test]
    fn a_card_too_small_for_fat16_is_refused_rather_than_mislabelled() {
        assert!(matches!(plan(64 * 1024), Err(FatError::TooSmall(_))));
    }
}
