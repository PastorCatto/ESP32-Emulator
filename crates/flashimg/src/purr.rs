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
/// First four bytes of a filled slot (u32 0x56524450, little-endian). A slot
/// whose header starts with 0xFF bytes is empty.
pub const DRIVER_SLOT_MAGIC: &[u8; 4] = b"PDRV";
/// Slot header, 64 bytes, little-endian: magic u32 @0, version u16 @4 (1),
/// flags u16 @6 (0), length u32 @8 (1..=`DRIVER_MAX_LEN`), crc32 u32 @12 (zlib,
/// of the .cat that starts at +64), file[32] @16 (NUL-terminated, printable
/// ASCII, no '/' or '\\'), reserved[16] @48 (0xFF).
pub const DRIVER_HEADER_LEN: usize = 64;
pub const DRIVER_MAX_LEN: usize = DRIVER_SLOT_SIZE - DRIVER_HEADER_LEN;
pub const DRIVER_SLOT_VERSION: u16 = 1;

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
/// shorter one leaves the rest of the partition erased. Each filled slot's
/// header is checked in full (magic, version, flags, length, file name,
/// reserved bytes, and the crc32 of the .cat); an empty slot is accepted.
/// Signatures are the device's job, as they are for every module.
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
            check_slot(slot).map_err(|why| layout(format!("slot {n} of the drivers image: {why}")))?;
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

/// zlib's crc32 (reflected, polynomial 0xEDB88320, init and final xor 0xFFFFFFFF).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Check a filled slot's 64-byte header against its .cat. `slot` is the whole
/// 8 KB slot.
fn check_slot(slot: &[u8]) -> std::result::Result<(), String> {
    let u16_at = |at: usize| u16::from_le_bytes([slot[at], slot[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([slot[at], slot[at + 1], slot[at + 2], slot[at + 3]]);

    if u16_at(4) != DRIVER_SLOT_VERSION {
        return Err(format!("version {} (expected {DRIVER_SLOT_VERSION})", u16_at(4)));
    }
    if u16_at(6) != 0 {
        return Err(format!("flags {:#06x} (expected 0)", u16_at(6)));
    }
    let len = u32_at(8) as usize;
    if !(1..=DRIVER_MAX_LEN).contains(&len) {
        return Err(format!("length {len} is outside 1..={DRIVER_MAX_LEN}"));
    }
    let name = &slot[16..48];
    let end = name.iter().position(|&b| b == 0).ok_or("file name is not NUL-terminated")?;
    if end == 0 {
        return Err("file name is empty".into());
    }
    if !name[..end].iter().all(|&b| (0x20..0x7f).contains(&b) && b != b'/' && b != b'\\') {
        return Err("file name has a character that is not printable ASCII, or is '/' or '\\'".into());
    }
    if slot[48..64].iter().any(|&b| b != 0xff) {
        return Err("reserved bytes are not 0xFF".into());
    }
    let cat = &slot[DRIVER_HEADER_LEN..DRIVER_HEADER_LEN + len];
    if crc32(cat) != u32_at(12) {
        return Err(format!("crc32 of the .cat is {:#010x}, header says {:#010x}", crc32(cat), u32_at(12)));
    }
    Ok(())
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

    /// A valid slot: header per SPEC 4a, a 4-byte .cat of `[fill, n, 3, 4]`.
    fn slot(fill: u8, n: usize) -> Vec<u8> {
        let cat = [fill, n as u8, 3, 4];
        let mut s = vec![0xffu8; DRIVER_SLOT_SIZE];
        s[..4].copy_from_slice(DRIVER_SLOT_MAGIC);
        s[4..6].copy_from_slice(&1u16.to_le_bytes());
        s[6..8].copy_from_slice(&0u16.to_le_bytes());
        s[8..12].copy_from_slice(&(cat.len() as u32).to_le_bytes());
        s[12..16].copy_from_slice(&crc32(&cat).to_le_bytes());
        s[16..48].fill(0);
        s[16..16 + 7].copy_from_slice(b"w5500.c");
        s[64..68].copy_from_slice(&cat);
        s
    }

    #[test]
    fn crc32_is_zlib_s() {
        // Golden from PURR OS's drvpart tests.
        assert_eq!(crc32(&[1, 2, 3, 4]), 0xb63c_fbcd);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn a_slot_with_a_bad_header_field_is_refused() {
        let cases: [(&str, fn(&mut [u8])); 7] = [
            ("version", |s| s[4] = 2),
            ("flags", |s| s[6] = 1),
            ("length", |s| s[8..12].copy_from_slice(&8129u32.to_le_bytes())),
            ("zero length", |s| s[8..12].copy_from_slice(&0u32.to_le_bytes())),
            ("path separator", |s| s[16] = b'/'),
            ("reserved", |s| s[50] = 0),
            ("crc", |s| s[64] ^= 1),
        ];
        for (what, damage) in cases {
            let mut flash = v3_flash();
            let mut s = slot(1, 0);
            damage(&mut s);
            let err = place_drivers(&mut flash, &s).unwrap_err();
            assert!(err.to_string().contains("slot 0"), "{what}: {err}");
        }
    }

    #[test]
    fn an_unterminated_file_name_is_refused() {
        let mut flash = v3_flash();
        let mut s = slot(1, 0);
        s[16..48].fill(b'a');
        assert!(place_drivers(&mut flash, &s).is_err());
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
