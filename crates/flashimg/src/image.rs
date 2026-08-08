//! `esp_image_header_t`, its segment headers, and the `esp_app_desc_t` that
//! ESP-IDF plants at a fixed offset inside every application image.

use crate::chip::{Chip, FlashSize};
use crate::error::{c_str, Error, Reader, Result};

pub const IMAGE_MAGIC: u8 = 0xe9;
pub const APP_DESC_MAGIC: u32 = 0xabcd_5432;

/// Size of `esp_image_header_t`.
pub const IMAGE_HEADER_LEN: usize = 24;
/// Size of `esp_image_segment_header_t`.
pub const SEGMENT_HEADER_LEN: usize = 8;
/// Size of `esp_app_desc_t`.
pub const APP_DESC_LEN: usize = 256;
/// `esp_app_desc_t` sits immediately after the image header and the first
/// segment header, so it always lands here.
pub const APP_DESC_OFFSET: usize = IMAGE_HEADER_LEN + SEGMENT_HEADER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiMode {
    Qio,
    Qout,
    Dio,
    Dout,
    FastReadDout,
    SlowRead,
    Opi,
    Unknown(u8),
}

impl SpiMode {
    fn from_raw(v: u8) -> Self {
        match v {
            0 => SpiMode::Qio,
            1 => SpiMode::Qout,
            2 => SpiMode::Dio,
            3 => SpiMode::Dout,
            4 => SpiMode::FastReadDout,
            5 => SpiMode::SlowRead,
            7 => SpiMode::Opi,
            other => SpiMode::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub load_addr: u32,
    pub len: u32,
    /// Offset of the segment payload within the app image.
    pub file_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageHeader {
    pub segment_count: u8,
    pub spi_mode: SpiMode,
    pub spi_speed: u8,
    pub flash_size: Option<FlashSize>,
    pub entry_addr: u32,
    pub chip: Chip,
    pub min_chip_rev_full: u16,
    pub max_chip_rev_full: u16,
    pub hash_appended: bool,
}

impl ImageHeader {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let magic = r.u8("image header")?;
        if magic != IMAGE_MAGIC {
            return Err(Error::BadImageMagic(magic));
        }
        let segment_count = r.u8("image header")?;
        let spi_mode = SpiMode::from_raw(r.u8("image header")?);
        let speed_size = r.u8("image header")?;
        let entry_addr = r.u32("image header")?;
        r.skip(1 + 3); // wp_pin, spi_pin_drv[3]
        let chip_id = r.u16("image header")?;
        let chip = Chip::from_id(chip_id).ok_or(Error::UnknownChip(chip_id))?;
        r.skip(1); // min_chip_rev (deprecated)
        let min_chip_rev_full = r.u16("image header")?;
        let max_chip_rev_full = r.u16("image header")?;
        r.skip(4); // reserved
        let hash_appended = r.u8("image header")? != 0;

        Ok(ImageHeader {
            segment_count,
            spi_mode,
            spi_speed: speed_size & 0x0f,
            flash_size: FlashSize::from_header_nibble(speed_size >> 4),
            entry_addr,
            chip,
            min_chip_rev_full,
            max_chip_rev_full,
            hash_appended,
        })
    }
}

/// The build-metadata block ESP-IDF embeds in every app.
///
/// `idf_version` is the field that matters most to us: the Wi-Fi driver ships
/// as a prebuilt static library, so this string identifies the exact machine
/// code of `esp_wifi_*` in this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDescriptor {
    pub secure_version: u32,
    pub app_version: String,
    pub project_name: String,
    pub build_time: String,
    pub build_date: String,
    pub idf_version: String,
    pub elf_sha256: [u8; 32],
}

impl AppDescriptor {
    /// Returns `Ok(None)` when the magic word is absent, which simply means the
    /// image was not built by ESP-IDF's app component (a bootloader, say).
    pub fn parse_at(buf: &[u8], offset: usize) -> Result<Option<Self>> {
        let Some(region) = buf.get(offset..offset + APP_DESC_LEN) else {
            return Ok(None);
        };
        let mut r = Reader::new(region);
        if r.u32("app descriptor")? != APP_DESC_MAGIC {
            return Ok(None);
        }
        let secure_version = r.u32("app descriptor")?;
        r.skip(8); // reserv1[2]
        let app_version = c_str(r.take(32, "app descriptor")?);
        let project_name = c_str(r.take(32, "app descriptor")?);
        let build_time = c_str(r.take(16, "app descriptor")?);
        let build_date = c_str(r.take(16, "app descriptor")?);
        let idf_version = c_str(r.take(32, "app descriptor")?);
        let mut elf_sha256 = [0u8; 32];
        elf_sha256.copy_from_slice(r.take(32, "app descriptor")?);

        Ok(Some(AppDescriptor {
            secure_version,
            app_version,
            project_name,
            build_time,
            build_date,
            idf_version,
            elf_sha256,
        }))
    }

    /// Parse `idf_version` ("v5.2.1-dirty", "v4.4.6-233-gabc") into
    /// `(major, minor, patch)`, ignoring any trailing describe suffix.
    pub fn idf_semver(&self) -> Option<(u16, u16, u16)> {
        let s = self.idf_version.trim_start_matches('v');
        let core = s.split(['-', '_']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch))
    }
}

/// A parsed ESP application image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppImage {
    pub header: ImageHeader,
    pub descriptor: Option<AppDescriptor>,
    pub segments: Vec<Segment>,
    /// Total on-flash length including checksum byte and optional SHA-256.
    pub total_len: usize,
}

impl AppImage {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = ImageHeader::parse(buf)?;
        let descriptor = AppDescriptor::parse_at(buf, APP_DESC_OFFSET)?;

        let mut segments = Vec::with_capacity(header.segment_count as usize);
        let mut r = Reader::at(buf, IMAGE_HEADER_LEN);
        for _ in 0..header.segment_count {
            let load_addr = r.u32("segment header")?;
            let len = r.u32("segment header")?;
            let file_offset = r.position();
            // Guard against a corrupt length steering us off the end.
            r.take(len as usize, "segment data")?;
            segments.push(Segment {
                load_addr,
                len,
                file_offset,
            });
        }

        // One checksum byte, positioned so the whole image is 16-byte aligned.
        let mut total_len = r.position();
        total_len += 1;
        total_len = total_len.div_ceil(16) * 16;
        if header.hash_appended {
            total_len += 32;
        }

        Ok(AppImage {
            header,
            descriptor,
            segments,
            total_len,
        })
    }

    /// Cheap check for "does this look like an app image at all", used to probe
    /// candidate offsets without paying for a full parse.
    pub fn looks_like_image(buf: &[u8]) -> bool {
        matches!(buf.first(), Some(&IMAGE_MAGIC))
            && buf.len() >= IMAGE_HEADER_LEN
            && Chip::from_id(u16::from_le_bytes([buf[12], buf[13]])).is_some()
    }
}
