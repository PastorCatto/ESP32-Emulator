//! The ESP-IDF partition table (`esp_partition_info_t` records at 0x8000).

use crate::error::{c_str, Error, Reader, Result};
use std::fmt;

pub const TABLE_OFFSET: u32 = 0x8000;
/// ESP-IDF reserves 3 KiB for the table regardless of how many entries it holds.
pub const TABLE_MAX_LEN: usize = 0xc00;
pub const ENTRY_LEN: usize = 32;

const ENTRY_MAGIC: u16 = 0x50aa;
const MD5_MAGIC: u16 = 0xebeb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionType {
    App,
    Data,
    Other(u8),
}

impl PartitionType {
    fn from_raw(v: u8) -> Self {
        match v {
            0x00 => PartitionType::App,
            0x01 => PartitionType::Data,
            other => PartitionType::Other(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            PartitionType::App => 0x00,
            PartitionType::Data => 0x01,
            PartitionType::Other(v) => v,
        }
    }
}

impl fmt::Display for PartitionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PartitionType::App => f.write_str("app"),
            PartitionType::Data => f.write_str("data"),
            PartitionType::Other(v) => write!(f, "{v:#04x}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub ty: PartitionType,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub label: String,
    pub flags: u32,
}

impl Partition {
    pub fn end(&self) -> u64 {
        u64::from(self.offset) + u64::from(self.size)
    }

    /// True for `factory` and any `ota_N` slot — the places a runnable app lives.
    pub fn is_bootable_app(&self) -> bool {
        self.ty == PartitionType::App && (self.subtype == 0x00 || (0x10..=0x1f).contains(&self.subtype))
    }

    /// Human-readable subtype, which is only meaningful alongside the type.
    pub fn subtype_name(&self) -> String {
        match (self.ty, self.subtype) {
            (PartitionType::App, 0x00) => "factory".into(),
            (PartitionType::App, 0x20) => "test".into(),
            (PartitionType::App, s) if (0x10..=0x1f).contains(&s) => format!("ota_{}", s - 0x10),
            (PartitionType::Data, 0x00) => "ota".into(),
            (PartitionType::Data, 0x01) => "phy".into(),
            (PartitionType::Data, 0x02) => "nvs".into(),
            (PartitionType::Data, 0x03) => "coredump".into(),
            (PartitionType::Data, 0x04) => "nvs_keys".into(),
            (PartitionType::Data, 0x05) => "efuse_em".into(),
            (PartitionType::Data, 0x81) => "fat".into(),
            (PartitionType::Data, 0x82) => "spiffs".into(),
            (PartitionType::Data, 0x83) => "littlefs".into(),
            (_, s) => format!("{s:#04x}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionTable {
    pub entries: Vec<Partition>,
}

impl PartitionTable {
    /// Parse entries until the terminator, an MD5 record, or blank flash.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut r = Reader::new(buf);

        while let Ok(chunk) = r.take(ENTRY_LEN, "partition entry") {
            // Erased flash terminates the table.
            if chunk.iter().all(|&b| b == 0xff) {
                break;
            }
            let magic = u16::from_le_bytes([chunk[0], chunk[1]]);
            match magic {
                ENTRY_MAGIC => {}
                // The MD5 checksum record always trails the real entries.
                MD5_MAGIC => break,
                other => return Err(Error::BadPartitionMagic(other)),
            }

            let mut e = Reader::at(chunk, 2);
            let ty = PartitionType::from_raw(e.u8("partition entry")?);
            let subtype = e.u8("partition entry")?;
            let offset = e.u32("partition entry")?;
            let size = e.u32("partition entry")?;
            let label = c_str(e.take(16, "partition entry")?);
            let flags = e.u32("partition entry")?;

            entries.push(Partition {
                ty,
                subtype,
                offset,
                size,
                label,
                flags,
            });
        }

        Ok(PartitionTable { entries })
    }

    /// Does a partition table plausibly start here?
    pub fn probe(buf: &[u8]) -> bool {
        buf.len() >= 2 && u16::from_le_bytes([buf[0], buf[1]]) == ENTRY_MAGIC
    }

    /// The partition the bootloader will run by default.
    ///
    /// We take `factory` when present, else the lowest-numbered OTA slot. This
    /// intentionally ignores the OTA data partition: without executing the
    /// bootloader we cannot know which slot it would actually pick.
    pub fn default_app(&self) -> Option<&Partition> {
        self.entries
            .iter()
            .find(|p| p.ty == PartitionType::App && p.subtype == 0x00)
            .or_else(|| {
                self.entries
                    .iter()
                    .filter(|p| p.is_bootable_app())
                    .min_by_key(|p| p.subtype)
            })
    }

    pub fn find(&self, label: &str) -> Option<&Partition> {
        self.entries.iter().find(|p| p.label == label)
    }

    /// Check the table is internally consistent and fits the given flash size.
    pub fn validate(&self, flash_size: u64) -> Result<()> {
        for p in &self.entries {
            if p.end() > flash_size {
                return Err(Error::PartitionOutOfBounds {
                    label: p.label.clone(),
                    end: p.end(),
                    flash: flash_size,
                });
            }
        }

        let mut sorted: Vec<&Partition> = self.entries.iter().collect();
        sorted.sort_by_key(|p| p.offset);
        for pair in sorted.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a.end() > u64::from(b.offset) {
                return Err(Error::PartitionOverlap {
                    a: a.label.clone(),
                    b: b.label.clone(),
                });
            }
        }
        Ok(())
    }

    /// Serialize the entries followed by the MD5 record.
    ///
    /// The record is not optional. Without it ESP-IDF does not skip the check,
    /// it rejects the whole table and the app loses every partition:
    ///
    /// ```text
    /// partition: No MD5 found in partition table
    /// partition: load_partitions returned 0x105
    /// ```
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.entries.len() + 1) * ENTRY_LEN);
        for p in &self.entries {
            out.extend_from_slice(&ENTRY_MAGIC.to_le_bytes());
            out.push(p.ty.raw());
            out.push(p.subtype);
            out.extend_from_slice(&p.offset.to_le_bytes());
            out.extend_from_slice(&p.size.to_le_bytes());
            let mut label = [0u8; 16];
            let bytes = p.label.as_bytes();
            let n = bytes.len().min(16);
            label[..n].copy_from_slice(&bytes[..n]);
            out.extend_from_slice(&label);
            out.extend_from_slice(&p.flags.to_le_bytes());
        }

        // 0xEBEB, fourteen erased bytes, then the digest of the entries above.
        let digest = crate::md5::md5(&out);
        out.extend_from_slice(&MD5_MAGIC.to_le_bytes());
        out.extend_from_slice(&[0xff; 14]);
        out.extend_from_slice(&digest);
        out
    }
}
