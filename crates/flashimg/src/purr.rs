//! PURR OS's flash map, for the T-Deck Plus (16 MB).
//!
//! The emulator does not need any of this to boot an image: the partition
//! table is read out of the image, and provisioning follows the table. This
//! module exists so the layout can be *built* and *checked* here, and so the
//! `drivers` partition (flash map v3) can be filled from a file.
//!
//! The authority is PURR OS's `PurrOS/partitions/tdeck_plus.csv` and the golden
//! table `sdk/tests/data/partition-table-tdeck_plus.bin` in that repository;
//! `testdata/partition-table-tdeck_plus.bin` here is a copy of the latter, and
//! a unit test pins [`PartitionTable::purr_tdeck_plus`] to it. If PURR OS
//! changes the map, copy the new golden table over and fix the constants.
//!
//! History: v2 (1.0.29) ended with `root` at 0x5C1000..0x1000000. v3
//! (2026-11-19) takes the last 0x14000 bytes for `drivers` and shortens `root`
//! to match. The 8 MB waveshare154 board keeps its own table and has no
//! `drivers` partition; it is not described here.

use crate::error::{Error, Result};
use crate::partition::{Partition, PartitionTable, PartitionType};
use std::fmt;

/// `root`: the LittleFS root filesystem. Its start did not move between v2 and v3.
pub const ROOT_OFFSET: u32 = 0x5c1000;
pub const ROOT_SIZE_V2: u32 = 0xa3f000;
pub const ROOT_SIZE_V3: u32 = 0xa2b000;

/// `drivers`: raw, data subtype 0x42, the last bytes of the 16 MB flash.
pub const DRIVERS_OFFSET: u32 = 0xfec000;
pub const DRIVERS_SIZE: u32 = 0x14000;
pub const DRIVERS_SUBTYPE: u8 = 0x42;
/// Ten slots of 8 KB, one signed driver module each (PURR OS `Drivers/SPEC.md`
/// section 4a). Slot n starts at n * `DRIVER_SLOT_SIZE`.
pub const DRIVER_SLOTS: usize = 10;
pub const DRIVER_SLOT_SIZE: usize = 0x2000;
/// First four bytes of a filled slot. A slot whose header is all 0xFF is empty.
pub const DRIVER_SLOT_MAGIC: &[u8; 4] = b"PDRV";

const FLASH_16_MB: u32 = 0x100_0000;

/// Which of PURR OS's flash maps a table is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMap {
    /// `root` at 0x5C1000 + 0xA2B000, `drivers` at 0xFEC000 + 0x14000.
    V3,
    /// `root` at 0x5C1000 + 0xA3F000 and no `drivers`: the 1.0.29 table. A
    /// current kernel will not find its drivers on an image like this.
    V2,
    /// Neither: another firmware, an older PURR OS, or the 8 MB board.
    Other,
}

impl fmt::Display for FlashMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FlashMap::V3 => "PURR OS flash map v3",
            FlashMap::V2 => "PURR OS flash map v2 (no drivers partition)",
            FlashMap::Other => "not a PURR OS 16 MB flash map",
        })
    }
}

impl PartitionTable {
    /// Which PURR OS flash map this table is. Looks only at `root` and
    /// `drivers`, since those are what the two versions differ in.
    pub fn purr_flash_map(&self) -> FlashMap {
        let root = self.find("root").filter(|p| p.offset == ROOT_OFFSET);
        let drivers = self.entries.iter().find(|p| is_drivers(p));
        match (root.map(|p| p.size), drivers) {
            (Some(ROOT_SIZE_V3), Some(d)) if d.offset == DRIVERS_OFFSET && d.size == DRIVERS_SIZE => {
                FlashMap::V3
            }
            (Some(ROOT_SIZE_V2), None) => FlashMap::V2,
            _ => FlashMap::Other,
        }
    }

    /// The T-Deck Plus table, flash map v3, as PURR OS writes it. The table sits
    /// at 0xA000 in the image (the bootloader outgrew 0x8000).
    pub fn purr_tdeck_plus() -> Self {
        let p = |ty, subtype, label: &str, offset, size| Partition {
            ty,
            subtype,
            offset,
            size,
            label: label.into(),
            flags: 0,
        };
        use PartitionType::{App, Data};
        PartitionTable {
            entries: vec![
                p(Data, 0x02, "nvs", 0xb000, 0x4000),
                p(Data, 0x40, "purrcfg", 0xf000, 0x2000),
                p(Data, 0x01, "phy_init", 0x11000, 0x1000),
                p(Data, 0x40, "bootpkg", 0x12000, 0xe000),
                p(App, 0x00, "kittenos", 0x20000, 0x1c0000),
                p(App, 0x10, "kernel", 0x1e0000, 0x220000),
                p(App, 0x20, "loader", 0x400000, 0x1c0000),
                p(Data, 0x41, "netrec", 0x5c0000, 0x1000),
                p(Data, 0x83, "root", ROOT_OFFSET, ROOT_SIZE_V3),
                p(Data, DRIVERS_SUBTYPE, "drivers", DRIVERS_OFFSET, DRIVERS_SIZE),
            ],
        }
    }
}

fn is_drivers(p: &Partition) -> bool {
    p.ty == PartitionType::Data && p.subtype == DRIVERS_SUBTYPE
}

/// What [`place_drivers`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriversPlaced {
    pub offset: u32,
    /// Slots that carry a driver (header starts with `PDRV`).
    pub filled: usize,
}

impl fmt::Display for DriversPlaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "drivers partition at {:#08x}: {} of {DRIVER_SLOTS} slots filled",
            self.offset, self.filled
        )
    }
}

/// Write a drivers image (what `tools/release/make_drivers_partition.py` or the
/// SDK's `drivers-<board>.bin` holds) into the `drivers` partition of a whole
/// flash image. The partition is found through the image's own table, so a
/// table without one is an error rather than a guess at 0xFEC000.
///
/// The image must be a whole number of 8 KB slots and fit the partition; a
/// shorter one leaves the rest of the partition erased. The slot headers are
/// checked for shape only (`PDRV`, or blank). Signatures and the `.cat` crc are
/// the device's job, as they are for every module.
pub fn place_drivers(flash: &mut [u8], drivers: &[u8]) -> Result<DriversPlaced> {
    let layout = |why: String| Error::Layout(why);
    let table_at = PartitionTable::locate(flash)
        .ok_or_else(|| layout("no partition table in the flash image".into()))?;
    let table = PartitionTable::parse(&flash[table_at as usize..])?;
    let part = table
        .entries
        .iter()
        .find(|p| is_drivers(p))
        .ok_or_else(|| layout("the partition table has no drivers partition (data, subtype 0x42): not flash map v3".into()))?;

    if drivers.is_empty() || drivers.len() % DRIVER_SLOT_SIZE != 0 {
        return Err(layout(format!(
            "a drivers image is a whole number of {DRIVER_SLOT_SIZE}-byte slots; this one is {} bytes",
            drivers.len()
        )));
    }
    if drivers.len() > part.size as usize {
        return Err(Error::RegionTooSmall {
            what: "drivers image".into(),
            need: drivers.len(),
            have: part.size as usize,
        });
    }
    let (start, end) = (part.offset as usize, part.end() as usize);
    let flash_len = flash.len();
    let region = flash.get_mut(start..end).ok_or(Error::RegionTooSmall {
        what: "drivers partition".into(),
        need: end,
        have: flash_len,
    })?;

    let mut filled = 0;
    for (n, slot) in drivers.chunks(DRIVER_SLOT_SIZE).enumerate() {
        let head = &slot[..DRIVER_SLOT_MAGIC.len()];
        if head == DRIVER_SLOT_MAGIC {
            filled += 1;
        } else if !head.iter().all(|&b| b == 0xff) {
            return Err(layout(format!(
                "slot {n} of the drivers image is neither empty nor a driver slot (header {head:02x?}): not a drivers image?"
            )));
        }
    }
    region.fill(0xff);
    region[..drivers.len()].copy_from_slice(drivers);
    Ok(DriversPlaced { offset: part.offset, filled })
}

/// Size a T-Deck Plus image must be. The table does not fit anything smaller.
pub const TDECK_PLUS_FLASH: u32 = FLASH_16_MB;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::littlefs;
    use crate::partition::TABLE_MAX_LEN;
    use crate::provision::{self, Action};

    const GOLDEN: &[u8] = include_bytes!("../testdata/partition-table-tdeck_plus.bin");
    const TABLE_AT: usize = 0xa000;

    fn v3_flash() -> Vec<u8> {
        let mut flash = vec![0xffu8; TDECK_PLUS_FLASH as usize];
        let t = PartitionTable::purr_tdeck_plus().serialize();
        flash[TABLE_AT..TABLE_AT + t.len()].copy_from_slice(&t);
        flash
    }

    fn slot(fill: u8, n: usize) -> Vec<u8> {
        let mut s = vec![0xffu8; DRIVER_SLOT_SIZE];
        s[..4].copy_from_slice(DRIVER_SLOT_MAGIC);
        s[4] = fill;
        s[5] = n as u8;
        s
    }

    #[test]
    fn the_built_table_is_byte_for_byte_the_golden_one() {
        assert_eq!(PartitionTable::purr_tdeck_plus().serialize(), GOLDEN);
    }

    #[test]
    fn the_golden_table_parses_to_the_built_one_and_is_v3() {
        let parsed = PartitionTable::parse(GOLDEN).unwrap();
        assert_eq!(parsed, PartitionTable::purr_tdeck_plus());
        assert_eq!(parsed.purr_flash_map(), FlashMap::V3);
        parsed.validate(u64::from(TDECK_PLUS_FLASH)).unwrap();
        assert!(GOLDEN.len() <= TABLE_MAX_LEN);
    }

    #[test]
    fn the_map_fills_the_flash_exactly() {
        let t = PartitionTable::purr_tdeck_plus();
        let drivers = t.find("drivers").unwrap();
        assert_eq!(drivers.end(), u64::from(TDECK_PLUS_FLASH));
        assert_eq!(t.find("root").unwrap().end(), u64::from(DRIVERS_OFFSET));
        assert_eq!(DRIVERS_SIZE as usize, DRIVER_SLOTS * DRIVER_SLOT_SIZE);
    }

    #[test]
    fn a_v2_table_is_recognised_as_v2() {
        let mut t = PartitionTable::purr_tdeck_plus();
        t.entries.pop();
        t.entries.last_mut().unwrap().size = ROOT_SIZE_V2;
        assert_eq!(t.purr_flash_map(), FlashMap::V2);
        t.validate(u64::from(TDECK_PLUS_FLASH)).unwrap();
    }

    #[test]
    fn other_tables_are_other() {
        assert_eq!(PartitionTable::default().purr_flash_map(), FlashMap::Other);
        let mut t = PartitionTable::purr_tdeck_plus();
        t.entries.iter_mut().find(|p| p.label == "drivers").unwrap().offset = 0xfe0000;
        assert_eq!(t.purr_flash_map(), FlashMap::Other);
    }

    #[test]
    fn a_blank_root_is_formatted_to_the_v3_size_and_drivers_is_left_blank() {
        let mut flash = v3_flash();
        let actions = provision::prepare(&mut flash);
        assert_eq!(
            actions,
            [Action::Formatted { label: "root".into(), offset: ROOT_OFFSET, size: ROOT_SIZE_V3 }]
        );
        let root = &flash[ROOT_OFFSET as usize..DRIVERS_OFFSET as usize];
        assert_eq!(root.len(), ROOT_SIZE_V3 as usize);
        assert!(littlefs::is_formatted(root));
        // The superblock records the block count of the *shortened* root.
        let blocks = littlefs::block_count(ROOT_SIZE_V3);
        assert_eq!(blocks, 0xa2b);
        assert!(
            root.windows(4).take(256).any(|w| w == blocks.to_le_bytes()),
            "block count {blocks:#x} not found in the superblock"
        );
        assert!(flash[DRIVERS_OFFSET as usize..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn placing_drivers_fills_the_partition_and_touches_nothing_else() {
        let mut flash = v3_flash();
        provision::prepare(&mut flash);
        let before = flash.clone();

        let mut image = Vec::new();
        for n in 0..3 {
            image.extend(slot(0xa0, n));
        }
        image.extend(vec![0xffu8; DRIVER_SLOT_SIZE * 7]);
        let placed = place_drivers(&mut flash, &image).unwrap();

        assert_eq!(placed, DriversPlaced { offset: DRIVERS_OFFSET, filled: 3 });
        assert_eq!(&flash[DRIVERS_OFFSET as usize..], &image[..]);
        assert_eq!(&flash[..DRIVERS_OFFSET as usize], &before[..DRIVERS_OFFSET as usize]);
    }

    #[test]
    fn a_shorter_image_erases_the_rest_of_the_partition() {
        let mut flash = v3_flash();
        flash[DRIVERS_OFFSET as usize + 0x10000] = 0x00;
        let placed = place_drivers(&mut flash, &slot(1, 0)).unwrap();
        assert_eq!(placed.filled, 1);
        assert!(flash[DRIVERS_OFFSET as usize + DRIVER_SLOT_SIZE..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn drivers_are_refused_without_a_drivers_partition() {
        let mut flash = v3_flash();
        let mut t = PartitionTable::purr_tdeck_plus();
        t.entries.pop();
        t.entries.last_mut().unwrap().size = ROOT_SIZE_V2;
        let bytes = t.serialize();
        flash[TABLE_AT..TABLE_AT + 0x100].fill(0xff);
        flash[TABLE_AT..TABLE_AT + bytes.len()].copy_from_slice(&bytes);
        let err = place_drivers(&mut flash, &slot(1, 0)).unwrap_err();
        assert!(err.to_string().contains("not flash map v3"), "{err}");
    }

    #[test]
    fn a_bad_drivers_image_is_refused_and_nothing_is_written() {
        let mut flash = v3_flash();
        let before = flash.clone();
        // Not a whole number of slots.
        assert!(place_drivers(&mut flash, &slot(1, 0)[..100]).is_err());
        assert!(place_drivers(&mut flash, &[]).is_err());
        // Too big.
        let big = vec![0xffu8; DRIVERS_SIZE as usize + DRIVER_SLOT_SIZE];
        assert!(matches!(place_drivers(&mut flash, &big), Err(Error::RegionTooSmall { .. })));
        // A slot that is neither blank nor PDRV: some other file.
        let mut junk = slot(1, 0);
        junk[0] = b'X';
        assert!(place_drivers(&mut flash, &junk).is_err());
        assert_eq!(flash, before);
    }

    #[test]
    fn a_flash_with_no_table_is_refused() {
        let mut flash = vec![0xffu8; TDECK_PLUS_FLASH as usize];
        assert!(place_drivers(&mut flash, &slot(1, 0)).is_err());
    }
}
