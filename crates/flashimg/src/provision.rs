//! Making a firmware's filesystem partition mountable before it boots.
//!
//! Released firmware is usually two downloads: the application, and a
//! filesystem image holding its assets. Drop only the application and the
//! filesystem partition is erased flash, which fails to mount. What happens
//! next depends entirely on the firmware — PURR OS ships its filesystem inside
//! the merged image and is fine, Bruce stops at a mount error, and the T-Deck
//! Launcher does not declare a filesystem partition at all yet still tries to
//! mount one.
//!
//! On real hardware you would flash the missing image. Here we can do better
//! than failing: format the partition so it mounts empty, and add one when the
//! table is missing it. An empty filesystem is not the same as the real assets,
//! so anything that reads a bundled file still will not find it — but the
//! firmware boots and draws, which is the difference between a black screen and
//! a usable emulator.

use crate::littlefs;
use crate::partition::{Partition, PartitionTable, PartitionType, ENTRY_LEN, TABLE_OFFSET};

/// Data subtypes that hold a filesystem we know how to create.
const SUBTYPE_SPIFFS: u8 = 0x82;
const SUBTYPE_LITTLEFS: u8 = 0x83;

/// Size to give a partition we are inventing. Large enough to be useful, small
/// enough to fit in the tail of a modestly-sized flash.
const INVENTED_SIZE: u32 = 0x100000;

/// Flash sector. Partitions have to start on one.
const SECTOR: u32 = 0x1000;

/// What the pass did, for the UI and the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The partition was erased, and now holds an empty volume.
    Formatted { label: String, offset: u32, size: u32 },
    /// No filesystem partition was declared, so one was appended and formatted.
    Added { label: String, offset: u32, size: u32 },
    /// A filesystem is already there and was left alone.
    Kept { label: String },
    /// We wanted to add a partition but could not.
    Declined { reason: String },
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Formatted { label, size, .. } => {
                write!(f, "formatted empty LittleFS in '{label}' ({} KiB)", size / 1024)
            }
            Action::Added { label, offset, size } => write!(
                f,
                "added '{label}' at {offset:#08x} ({} KiB) and formatted it",
                size / 1024
            ),
            Action::Kept { label } => write!(f, "'{label}' already holds a filesystem"),
            Action::Declined { reason } => write!(f, "no filesystem partition: {reason}"),
        }
    }
}

/// Is every byte erased? A partition that was never written reads as all ones.
fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0xff)
}

/// Filesystem partitions in table order.
fn filesystems(table: &PartitionTable) -> Vec<&Partition> {
    table
        .entries
        .iter()
        .filter(|p| {
            p.ty == PartitionType::Data && matches!(p.subtype, SUBTYPE_SPIFFS | SUBTYPE_LITTLEFS)
        })
        .collect()
}

/// Somewhere to put a partition we are inventing: after everything already
/// declared, sector-aligned, inside the flash.
fn free_tail(table: &PartitionTable, flash: u32) -> Option<(u32, u32)> {
    let used = table.entries.iter().map(|p| p.end()).max().unwrap_or(0) as u32;
    let start = used.next_multiple_of(SECTOR);
    let available = flash.checked_sub(start)?;
    // Below two blocks there is nowhere to put the dir pair, and a volume that
    // small would be useless anyway.
    if available < 8 * littlefs::BLOCK_SIZE {
        return None;
    }
    Some((start, available.min(INVENTED_SIZE)))
}

/// Format blank filesystem partitions, and add one if the firmware declares
/// none. `flash` is the whole assembled image and is modified in place.
///
/// Firmware that already carries a filesystem is untouched, so this is safe to
/// run over every image.
pub fn prepare(flash: &mut Vec<u8>) -> Vec<Action> {
    let flash_size = flash.len() as u32;
    let Some(table_bytes) = flash.get(TABLE_OFFSET as usize..) else {
        return Vec::new();
    };
    let Ok(table) = PartitionTable::parse(table_bytes) else {
        return Vec::new();
    };

    // Erased flash parses as a table with no entries. That is not a firmware
    // missing its filesystem, it is not a firmware at all.
    if table.entries.is_empty() {
        return Vec::new();
    }

    let declared = filesystems(&table);
    if declared.is_empty() {
        return match invent(flash, &table, flash_size) {
            Ok(action) | Err(action) => vec![action],
        };
    }

    let mut actions = Vec::new();
    for part in declared {
        let (start, end) = (part.offset as usize, part.end() as usize);
        // A partition can legitimately sit beyond the end of a short image;
        // grow to meet it, since the flash is that size on the device.
        if end > flash.len() {
            flash.resize(end, 0xff);
        }
        let region = &mut flash[start..end];
        if is_blank(region) {
            littlefs::format_in_place(region);
            actions.push(Action::Formatted {
                label: part.label.clone(),
                offset: part.offset,
                size: part.size,
            });
        } else {
            actions.push(Action::Kept { label: part.label.clone() });
        }
    }
    actions
}

/// Append a filesystem partition to a table that has none.
///
/// The rewritten table has to carry an MD5 record. Leaving it off does not make
/// ESP-IDF skip the check, it makes it reject the table outright:
///
/// ```text
/// partition: No MD5 found in partition table
/// partition: load_partitions returned 0x105
/// ```
///
/// which costs the app every partition, including the one it boots from.
fn invent(flash: &mut [u8], table: &PartitionTable, flash_size: u32) -> Result<Action, Action> {
    let Some((offset, size)) = free_tail(table, flash_size) else {
        return Err(Action::Declined { reason: "no room left in flash".into() });
    };

    let mut grown = table.clone();
    grown.entries.push(Partition {
        ty: PartitionType::Data,
        // Subtype `spiffs` rather than `littlefs`: esp_littlefs looks for
        // `spiffs` by default, and every firmware here labels it that way.
        subtype: SUBTYPE_SPIFFS,
        offset,
        size,
        label: "spiffs".into(),
        flags: 0,
    });

    // serialize() appends the MD5 record itself.
    let serialized = grown.serialize();
    let at = TABLE_OFFSET as usize;
    // The table lives in one sector and must not run into what follows.
    if serialized.len() + ENTRY_LEN > SECTOR as usize {
        return Err(Action::Declined { reason: "partition table is full".into() });
    }
    let Some(slot) = flash.get_mut(at..at + serialized.len()) else {
        return Err(Action::Declined { reason: "image is shorter than its table".into() });
    };
    slot.copy_from_slice(&serialized);
    // Erase whatever followed, including any MD5 record, so the terminator is
    // the blank flash right after the entry we just added.
    let tail = at + serialized.len();
    let sector_end = (tail).next_multiple_of(SECTOR as usize).min(flash.len());
    flash[tail..sector_end].fill(0xff);

    littlefs::format_in_place(&mut flash[offset as usize..(offset + size) as usize]);
    Ok(Action::Added { label: "spiffs".into(), offset, size })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chip, FlashSize};

    /// A flash image with the given entries and nothing else written.
    fn flash_with(entries: Vec<Partition>, size: u32) -> Vec<u8> {
        let mut flash = vec![0xffu8; size as usize];
        let table = PartitionTable { entries };
        let bytes = table.serialize();
        let at = TABLE_OFFSET as usize;
        flash[at..at + bytes.len()].copy_from_slice(&bytes);
        flash
    }

    fn data(label: &str, subtype: u8, offset: u32, size: u32) -> Partition {
        Partition {
            ty: PartitionType::Data,
            subtype,
            offset,
            size,
            label: label.into(),
            flags: 0,
        }
    }

    fn app(offset: u32, size: u32) -> Partition {
        Partition {
            ty: PartitionType::App,
            subtype: 0,
            offset,
            size,
            label: "factory".into(),
            flags: 0,
        }
    }

    #[test]
    fn a_blank_partition_is_formatted() {
        // Bruce: the spiffs partition is past the end of the download.
        let mut flash = flash_with(
            vec![app(0x10000, 0x470000), data("spiffs", 0x82, 0x480000, 0x100000)],
            0x1000000,
        );
        let actions = prepare(&mut flash);
        assert!(matches!(actions[..], [Action::Formatted { .. }]), "{actions:?}");
        assert!(littlefs::is_formatted(&flash[0x480000..]));
    }

    #[test]
    fn an_existing_filesystem_is_left_alone() {
        // PURR OS ships its filesystem inside the merged image. Overwriting it
        // would replace the firmware's own assets with nothing.
        let mut flash = flash_with(
            vec![app(0x10000, 0x470000), data("spiffs", 0x82, 0x480000, 0x100000)],
            0x1000000,
        );
        flash[0x480000..0x480010].copy_from_slice(b"real filesystem!");
        let actions = prepare(&mut flash);
        assert!(matches!(actions[..], [Action::Kept { .. }]), "{actions:?}");
        assert_eq!(&flash[0x480000..0x480010], b"real filesystem!");
    }

    #[test]
    fn a_partition_past_the_end_of_a_short_image_grows_it() {
        let mut flash = flash_with(
            vec![app(0x10000, 0x470000), data("spiffs", 0x82, 0x480000, 0x100000)],
            0x490000,
        );
        prepare(&mut flash);
        assert_eq!(flash.len(), 0x580000);
        assert!(littlefs::is_formatted(&flash[0x480000..]));
    }

    #[test]
    fn a_missing_partition_is_added() {
        // The Launcher declares no filesystem at all.
        let mut flash = flash_with(
            vec![data("nvs", 0x02, 0x9000, 0x5000), app(0x10000, 0x300000)],
            0x1000000,
        );
        let actions = prepare(&mut flash);
        let Action::Added { offset, size, .. } = &actions[0] else {
            panic!("{actions:?}");
        };
        assert_eq!(*offset, 0x310000);
        assert_eq!(*size, INVENTED_SIZE);
        assert!(littlefs::is_formatted(&flash[*offset as usize..]));
    }

    #[test]
    fn the_added_partition_reads_back_from_the_table() {
        let mut flash = flash_with(
            vec![data("nvs", 0x02, 0x9000, 0x5000), app(0x10000, 0x300000)],
            0x1000000,
        );
        prepare(&mut flash);
        let table = PartitionTable::parse(&flash[TABLE_OFFSET as usize..]).unwrap();
        assert_eq!(table.entries.len(), 3);
        let added = table.find("spiffs").expect("must be in the table");
        assert_eq!(added.subtype, SUBTYPE_SPIFFS);
        assert_eq!(added.offset, 0x310000);
        table.validate(u64::from(0x1000000u32)).expect("table must stay valid");
    }

    #[test]
    fn adding_is_declined_when_the_flash_is_full() {
        let mut flash = flash_with(vec![app(0x10000, 0x3f0000)], 0x400000);
        let actions = prepare(&mut flash);
        assert!(matches!(actions[..], [Action::Declined { .. }]), "{actions:?}");
    }

    #[test]
    fn an_invented_partition_never_overlaps_what_is_declared() {
        let mut flash = flash_with(
            vec![data("nvs", 0x02, 0x9000, 0x5000), app(0x10000, 0x300001)],
            0x1000000,
        );
        prepare(&mut flash);
        let table = PartitionTable::parse(&flash[TABLE_OFFSET as usize..]).unwrap();
        table.validate(0x1000000).expect("must not overlap");
    }

    #[test]
    fn an_image_with_no_table_is_left_alone() {
        let mut flash = vec![0xffu8; 0x100000];
        assert!(prepare(&mut flash).is_empty());
        assert!(flash.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_blank_image_is_untouched() {
        // Nothing to boot means nothing to provision.
        let image = crate::FlashImage::blank(Chip::Esp32S3, FlashSize(0x400000));
        let mut flash = image.as_bytes().to_vec();
        let before = flash.clone();
        prepare(&mut flash);
        assert_eq!(flash, before);
    }
}
