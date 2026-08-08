//! Working out what the user just dropped on the window, and turning it into a
//! flash image QEMU can boot.

use crate::chip::{Chip, FlashSize};
use crate::error::{Error, Result};
use crate::image::AppImage;
use crate::partition::{Partition, PartitionTable, PartitionType, TABLE_MAX_LEN, TABLE_OFFSET};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

/// A complete flash image: bootloader, partition table, and app all present at
/// their absolute offsets. This is `esptool merge_bin` output, and it is what
/// most projects publish as their release artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedFlash {
    pub chip: Chip,
    pub bootloader: AppImage,
    pub table: PartitionTable,
    /// The app the bootloader would run, when we can locate and parse it.
    pub app: Option<AppImage>,
}

/// What a dropped file turned out to be.
///
/// The payload variants are boxed to keep this enum small: it is passed around
/// freely, and an inline `AppImage` would make every `Dropped` a few hundred
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dropped {
    MergedFlash(Box<MergedFlash>),
    /// A bare application image, meant to be written at an app partition.
    /// Bootable only once we supply a bootloader and a partition table.
    AppOnly(Box<AppImage>),
    /// A second-stage bootloader on its own.
    Bootloader(Box<AppImage>),
    /// An unstripped ELF. The best case for Wi-Fi patching: real symbols, so we
    /// can locate `esp_wifi_*` exactly instead of matching byte signatures.
    Elf,
    Unknown,
}

impl Dropped {
    pub fn identify(buf: &[u8]) -> Self {
        if buf.starts_with(ELF_MAGIC) {
            return Dropped::Elf;
        }

        // A merged image is the only thing with a partition table at 0x8000.
        if let Some(tbl_region) = buf.get(TABLE_OFFSET as usize..) {
            if PartitionTable::probe(tbl_region) {
                let table_len = tbl_region.len().min(TABLE_MAX_LEN);
                if let Ok(table) = PartitionTable::parse(&tbl_region[..table_len]) {
                    // The bootloader sits at 0x0 or 0x1000 depending on the part.
                    for &off in &[0usize, 0x1000] {
                        let Some(region) = buf.get(off..) else { continue };
                        if !AppImage::looks_like_image(region) {
                            continue;
                        }
                        let Ok(bootloader) = AppImage::parse(region) else {
                            continue;
                        };
                        if bootloader.header.chip.bootloader_offset() as usize != off {
                            continue;
                        }
                        let chip = bootloader.header.chip;
                        let app = table
                            .default_app()
                            .and_then(|p| buf.get(p.offset as usize..))
                            .and_then(|r| AppImage::parse(r).ok());
                        return Dropped::MergedFlash(Box::new(MergedFlash {
                            chip,
                            bootloader,
                            table,
                            app,
                        }));
                    }
                }
            }
        }

        if AppImage::looks_like_image(buf) {
            if let Ok(img) = AppImage::parse(buf) {
                // Bootloaders carry no `esp_app_desc_t`; applications always do.
                return if img.descriptor.is_some() {
                    Dropped::AppOnly(Box::new(img))
                } else {
                    Dropped::Bootloader(Box::new(img))
                };
            }
        }

        Dropped::Unknown
    }

    pub fn chip(&self) -> Option<Chip> {
        match self {
            Dropped::MergedFlash(m) => Some(m.chip),
            Dropped::AppOnly(img) | Dropped::Bootloader(img) => Some(img.header.chip),
            Dropped::Elf | Dropped::Unknown => None,
        }
    }

    /// The application image, wherever it came from.
    fn app_image(&self) -> Option<&AppImage> {
        match self {
            Dropped::MergedFlash(m) => m.app.as_ref(),
            Dropped::AppOnly(img) => Some(img),
            _ => None,
        }
    }

    /// The IDF version this was built with, when we can tell. Keys the Wi-Fi
    /// signature database.
    pub fn idf_version(&self) -> Option<&str> {
        Some(self.app_image()?.descriptor.as_ref()?.idf_version.as_str())
    }

    pub fn project_name(&self) -> Option<&str> {
        Some(self.app_image()?.descriptor.as_ref()?.project_name.as_str())
    }
}

/// A flash device image, built up region by region.
#[derive(Debug, Clone)]
pub struct FlashImage {
    data: Vec<u8>,
    chip: Chip,
}

impl FlashImage {
    /// A blank device. Erased NOR flash reads as 0xFF, and the bootloader
    /// relies on that to spot unwritten regions, so we must not use zeroes.
    pub fn blank(chip: Chip, size: FlashSize) -> Self {
        FlashImage {
            data: vec![0xff; size.bytes() as usize],
            chip,
        }
    }

    pub fn chip(&self) -> Chip {
        self.chip
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn write_at(&mut self, offset: u32, bytes: &[u8], what: &str) -> Result<()> {
        let start = offset as usize;
        let end = start
            .checked_add(bytes.len())
            .ok_or_else(|| Error::RegionTooSmall {
                what: what.to_string(),
                need: bytes.len(),
                have: 0,
            })?;
        if end > self.data.len() {
            return Err(Error::RegionTooSmall {
                what: what.to_string(),
                need: end,
                have: self.data.len(),
            });
        }
        self.data[start..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Build a bootable image from whatever the user gave us.
    ///
    /// `bootloader` is only consulted when the dropped file did not already
    /// contain one; supplying a bootloader that matches the detected IDF
    /// version is the caller's job.
    pub fn assemble(
        dropped: &Dropped,
        raw: &[u8],
        size: FlashSize,
        bootloader: Option<&[u8]>,
    ) -> Result<Self> {
        match dropped {
            Dropped::MergedFlash(m) => {
                m.table.validate(u64::from(size.bytes()))?;
                let mut img = FlashImage::blank(m.chip, size);
                // A merged image is already laid out in absolute flash offsets,
                // so it drops in wholesale. Trailing bytes past our chosen flash
                // size would be unaddressable, so refuse rather than truncate.
                if raw.len() > img.data.len() {
                    return Err(Error::RegionTooSmall {
                        what: "merged flash image".into(),
                        need: raw.len(),
                        have: img.data.len(),
                    });
                }
                img.write_at(0, raw, "merged flash image")?;
                Ok(img)
            }

            Dropped::AppOnly(app) => {
                let chip = app.header.chip;
                let mut img = FlashImage::blank(chip, size);

                let bl = bootloader.ok_or_else(|| Error::RegionTooSmall {
                    what: "bootloader (dropped file is an app image with no bootloader)".into(),
                    need: 1,
                    have: 0,
                })?;
                img.write_at(chip.bootloader_offset(), bl, "bootloader")?;

                let table = PartitionTable::single_factory(size, raw.len() as u32);
                table.validate(u64::from(size.bytes()))?;
                let serialized = table.serialize();
                if serialized.len() > TABLE_MAX_LEN {
                    return Err(Error::RegionTooSmall {
                        what: "partition table".into(),
                        need: serialized.len(),
                        have: TABLE_MAX_LEN,
                    });
                }
                img.write_at(TABLE_OFFSET, &serialized, "partition table")?;

                let factory = table
                    .default_app()
                    .expect("single_factory always yields a factory partition");
                img.write_at(factory.offset, raw, "application")?;
                Ok(img)
            }

            Dropped::Bootloader(bl) => {
                let chip = bl.header.chip;
                let mut img = FlashImage::blank(chip, size);
                img.write_at(chip.bootloader_offset(), raw, "bootloader")?;
                Ok(img)
            }

            Dropped::Elf | Dropped::Unknown => Err(Error::RegionTooSmall {
                what: "recognizable firmware image".into(),
                need: 1,
                have: 0,
            }),
        }
    }
}

impl PartitionTable {
    /// The stock ESP-IDF "single factory app" layout, grown to fit `app_len`.
    ///
    /// We deliberately omit the trailing MD5 record: ESP-IDF skips the checksum
    /// when no MD5 entry is present, so leaving it out is valid and saves us
    /// carrying an MD5 implementation.
    pub fn single_factory(flash: FlashSize, app_len: u32) -> Self {
        const NVS_OFFSET: u32 = 0x9000;
        const NVS_SIZE: u32 = 0x5000;
        const PHY_OFFSET: u32 = 0xe000;
        const PHY_SIZE: u32 = 0x1000;
        const APP_OFFSET: u32 = 0x10000;
        // App partitions must start on a 64 KiB boundary.
        const APP_ALIGN: u32 = 0x10000;

        let min_app = app_len.next_multiple_of(APP_ALIGN).max(APP_ALIGN);
        // Give the app half the remaining space, so there is room for a
        // filesystem; clamp so a large app still fits.
        let available = flash.bytes().saturating_sub(APP_OFFSET);
        let app_size = (available / 2).next_multiple_of(APP_ALIGN).max(min_app).min(available);

        let mut entries = vec![
            Partition {
                ty: PartitionType::Data,
                subtype: 0x02,
                offset: NVS_OFFSET,
                size: NVS_SIZE,
                label: "nvs".into(),
                flags: 0,
            },
            Partition {
                ty: PartitionType::Data,
                subtype: 0x01,
                offset: PHY_OFFSET,
                size: PHY_SIZE,
                label: "phy_init".into(),
                flags: 0,
            },
            Partition {
                ty: PartitionType::App,
                subtype: 0x00,
                offset: APP_OFFSET,
                size: app_size,
                label: "factory".into(),
                flags: 0,
            },
        ];

        let spiffs_offset = APP_OFFSET + app_size;
        if let Some(spiffs_size) = flash.bytes().checked_sub(spiffs_offset).filter(|&s| s >= 0x10000)
        {
            entries.push(Partition {
                ty: PartitionType::Data,
                subtype: 0x82,
                offset: spiffs_offset,
                size: spiffs_size,
                label: "storage".into(),
                flags: 0,
            });
        }

        PartitionTable { entries }
    }
}
