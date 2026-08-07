//! Binary-format handling for ESP32 firmware.
//!
//! Given an arbitrary file dropped onto the emulator window, this crate works
//! out what it is (merged flash image, bare app, bootloader, ELF), extracts the
//! build metadata, and assembles a flash image the emulator can boot.

mod chip;
mod elf;
pub(crate) mod sha256;
pub mod patch;
mod error;
mod image;
mod layout;
mod partition;

pub use chip::{Chip, FlashSize, ALL_CHIPS};
/// Digest of an ELF, for checking it against the image built from it.
pub use sha256::sha256;
pub use elf::{ElfSymbols, Symbol, SymbolKind, EM_RISCV, EM_XTENSA};
pub use error::{Error, Result};
pub use image::{
    AppDescriptor, AppImage, ImageHeader, Segment, SpiMode, APP_DESC_LEN, APP_DESC_MAGIC,
    APP_DESC_OFFSET, IMAGE_HEADER_LEN, IMAGE_MAGIC, SEGMENT_HEADER_LEN,
};
pub use layout::{Dropped, FlashImage, MergedFlash};
pub use partition::{
    Partition, PartitionTable, PartitionType, ENTRY_LEN, TABLE_MAX_LEN, TABLE_OFFSET,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid app image.
    fn synth_app(chip: Chip, with_desc: bool, payload: usize) -> Vec<u8> {
        let mut v = vec![
            IMAGE_MAGIC,
            1,                 // one segment
            2,                 // spi_mode = DIO
            (4 << 4) | 0x0f,   // 16MB, speed nibble
        ];
        v.extend_from_slice(&0x4008_0000u32.to_le_bytes()); // entry
        v.push(0); // wp_pin
        v.extend_from_slice(&[0, 0, 0]); // spi_pin_drv
        v.extend_from_slice(&chip.id().to_le_bytes());
        v.push(0); // min_chip_rev
        v.extend_from_slice(&0u16.to_le_bytes()); // min_chip_rev_full
        v.extend_from_slice(&0xffffu16.to_le_bytes()); // max_chip_rev_full
        v.extend_from_slice(&[0; 4]); // reserved
        v.push(0); // hash_appended
        assert_eq!(v.len(), IMAGE_HEADER_LEN);

        // Segment header, then the payload that `esp_app_desc_t` lives at the
        // start of.
        let seg_len = payload.max(if with_desc { APP_DESC_LEN } else { 0 });
        v.extend_from_slice(&0x3fc8_0000u32.to_le_bytes()); // load_addr
        v.extend_from_slice(&(seg_len as u32).to_le_bytes());
        assert_eq!(v.len(), APP_DESC_OFFSET);

        let seg_start = v.len();
        if with_desc {
            v.extend_from_slice(&APP_DESC_MAGIC.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes()); // secure_version
            v.extend_from_slice(&[0; 8]); // reserv1
            let field = |s: &str, n: usize| {
                let mut b = vec![0u8; n];
                let src = s.as_bytes();
                b[..src.len()].copy_from_slice(src);
                b
            };
            v.extend_from_slice(&field("1.2.3", 32)); // version
            v.extend_from_slice(&field("meshtastic", 32)); // project_name
            v.extend_from_slice(&field("12:00:00", 16)); // time
            v.extend_from_slice(&field("Aug  3 2026", 16)); // date
            v.extend_from_slice(&field("v5.2.1-dirty", 32)); // idf_ver
            v.extend_from_slice(&[0xab; 32]); // elf sha256
            v.extend_from_slice(&[0; 80]); // reserv2
            assert_eq!(v.len() - seg_start, APP_DESC_LEN);
        }
        v.resize(seg_start + seg_len, 0);

        // Checksum byte, padded so the image is 16-byte aligned.
        v.push(0);
        while v.len() % 16 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_image_header() {
        let raw = synth_app(Chip::Esp32S3, true, 512);
        let img = AppImage::parse(&raw).expect("parse");
        assert_eq!(img.header.chip, Chip::Esp32S3);
        assert_eq!(img.header.entry_addr, 0x4008_0000);
        assert_eq!(img.header.flash_size, Some(FlashSize::MB16));
        assert_eq!(img.header.spi_mode, SpiMode::Dio);
        assert_eq!(img.segments.len(), 1);
        assert_eq!(img.total_len, raw.len());
    }

    #[test]
    fn extracts_build_metadata() {
        let raw = synth_app(Chip::Esp32S3, true, 512);
        let img = AppImage::parse(&raw).unwrap();
        let d = img.descriptor.expect("descriptor present");
        assert_eq!(d.project_name, "meshtastic");
        assert_eq!(d.app_version, "1.2.3");
        assert_eq!(d.idf_version, "v5.2.1-dirty");
        // The suffix must not defeat version keying for the Wi-Fi signature DB.
        assert_eq!(d.idf_semver(), Some((5, 2, 1)));
    }

    #[test]
    fn image_without_descriptor_is_a_bootloader() {
        let raw = synth_app(Chip::Esp32S3, false, 64);
        assert_eq!(
            Dropped::identify(&raw),
            Dropped::Bootloader(Box::new(AppImage::parse(&raw).unwrap()))
        );
    }

    #[test]
    fn rejects_non_esp_files() {
        assert_eq!(Dropped::identify(b"not firmware at all"), Dropped::Unknown);
        assert_eq!(Dropped::identify(b"\x7fELF\x02\x01\x01"), Dropped::Elf);
    }

    #[test]
    fn image_parse_survives_a_lying_segment_length() {
        let mut raw = synth_app(Chip::Esp32S3, true, 512);
        // Claim a 4 GiB segment in a 600-byte file.
        raw[IMAGE_HEADER_LEN + 4..IMAGE_HEADER_LEN + 8].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert!(matches!(AppImage::parse(&raw), Err(Error::Truncated { .. })));
    }

    #[test]
    fn partition_table_roundtrips() {
        let table = PartitionTable::single_factory(FlashSize::MB16, 0x30_0000);
        let bytes = table.serialize();
        let parsed = PartitionTable::parse(&bytes).expect("parse");
        assert_eq!(parsed, table);
        assert_eq!(parsed.default_app().unwrap().label, "factory");
        assert_eq!(parsed.find("nvs").unwrap().offset, 0x9000);
        table.validate(u64::from(FlashSize::MB16.bytes())).expect("valid");
    }

    #[test]
    fn partition_table_stops_at_erased_flash() {
        let mut bytes = PartitionTable::single_factory(FlashSize::MB16, 0x1000).serialize();
        let real = PartitionTable::parse(&bytes).unwrap().entries.len();
        bytes.resize(TABLE_MAX_LEN, 0xff);
        assert_eq!(PartitionTable::parse(&bytes).unwrap().entries.len(), real);
    }

    #[test]
    fn detects_overlapping_partitions() {
        let mut table = PartitionTable::single_factory(FlashSize::MB16, 0x1000);
        table.entries[0].size = 0x9000; // nvs now runs into phy_init
        assert!(matches!(
            table.validate(u64::from(FlashSize::MB16.bytes())),
            Err(Error::PartitionOverlap { .. })
        ));
    }

    #[test]
    fn identifies_and_assembles_a_merged_image() {
        let chip = Chip::Esp32S3;
        let bootloader = synth_app(chip, false, 0x1000);
        let app = synth_app(chip, true, 0x2000);
        let table = PartitionTable::single_factory(FlashSize::MB16, app.len() as u32);
        let app_off = table.default_app().unwrap().offset as usize;

        let mut merged = vec![0xffu8; app_off + app.len()];
        let bl_off = chip.bootloader_offset() as usize;
        merged[bl_off..bl_off + bootloader.len()].copy_from_slice(&bootloader);
        let tbl = table.serialize();
        merged[TABLE_OFFSET as usize..TABLE_OFFSET as usize + tbl.len()].copy_from_slice(&tbl);
        merged[app_off..app_off + app.len()].copy_from_slice(&app);

        let dropped = Dropped::identify(&merged);
        let Dropped::MergedFlash(m) = &dropped else {
            panic!("expected a merged image, got {dropped:?}");
        };
        assert_eq!(m.chip, chip);
        assert!(m.app.is_some(), "app should be located via the partition table");
        assert_eq!(dropped.idf_version(), Some("v5.2.1-dirty"));
        assert_eq!(dropped.project_name(), Some("meshtastic"));

        let flash = FlashImage::assemble(&dropped, &merged, FlashSize::MB16, None).expect("assemble");
        assert_eq!(flash.len(), FlashSize::MB16.bytes() as usize);
        // Everything past the supplied content must stay erased.
        assert!(flash.as_bytes()[merged.len()..].iter().all(|&b| b == 0xff));
        assert_eq!(&flash.as_bytes()[app_off..app_off + app.len()], &app[..]);
    }

    #[test]
    fn bare_app_needs_a_bootloader_and_then_assembles() {
        let app = synth_app(Chip::Esp32S3, true, 0x2000);
        let dropped = Dropped::identify(&app);
        assert!(matches!(dropped, Dropped::AppOnly(_)));

        // Without a bootloader we must fail loudly rather than emit junk.
        assert!(FlashImage::assemble(&dropped, &app, FlashSize::MB16, None).is_err());

        let bl = synth_app(Chip::Esp32S3, false, 0x1000);
        let flash = FlashImage::assemble(&dropped, &app, FlashSize::MB16, Some(&bl)).expect("assemble");
        let bl_off = Chip::Esp32S3.bootloader_offset() as usize;
        assert_eq!(&flash.as_bytes()[bl_off..bl_off + bl.len()], &bl[..]);
        assert!(PartitionTable::probe(&flash.as_bytes()[TABLE_OFFSET as usize..]));
    }

    #[test]
    fn assemble_refuses_an_image_larger_than_the_flash() {
        let chip = Chip::Esp32S3;
        let app = synth_app(chip, true, 0x2000);
        let dropped = Dropped::identify(&app);
        let tiny = FlashSize(0x1000);
        assert!(matches!(
            FlashImage::assemble(&dropped, &app, tiny, Some(&app)),
            Err(Error::RegionTooSmall { .. })
        ));
    }
}
