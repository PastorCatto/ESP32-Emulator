//! Chip identification and the per-chip flash constants we need to lay out an image.

use std::fmt;

/// The `chip_id` field of `esp_image_header_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chip {
    Esp32,
    Esp32S2,
    Esp32S3,
    Esp32C2,
    Esp32C3,
    Esp32C5,
    Esp32C6,
    Esp32H2,
    Esp32P4,
}

impl Chip {
    pub fn from_id(id: u16) -> Option<Self> {
        Some(match id {
            0x0000 => Chip::Esp32,
            0x0002 => Chip::Esp32S2,
            0x0005 => Chip::Esp32C3,
            0x0009 => Chip::Esp32S3,
            0x000c => Chip::Esp32C2,
            0x000d => Chip::Esp32C6,
            0x0010 => Chip::Esp32H2,
            0x0012 => Chip::Esp32P4,
            0x0017 => Chip::Esp32C5,
            _ => return None,
        })
    }

    pub fn id(self) -> u16 {
        match self {
            Chip::Esp32 => 0x0000,
            Chip::Esp32S2 => 0x0002,
            Chip::Esp32C3 => 0x0005,
            Chip::Esp32S3 => 0x0009,
            Chip::Esp32C2 => 0x000c,
            Chip::Esp32C6 => 0x000d,
            Chip::Esp32H2 => 0x0010,
            Chip::Esp32P4 => 0x0012,
            Chip::Esp32C5 => 0x0017,
        }
    }

    /// Where the second-stage bootloader lives in flash.
    ///
    /// The original ESP32 and S2 reserve the first 4 KiB; most later parts start
    /// the bootloader at zero, and the P4 reserves 8 KiB.
    pub fn bootloader_offset(self) -> u32 {
        match self {
            Chip::Esp32 | Chip::Esp32S2 => 0x1000,
            Chip::Esp32P4 => 0x2000,
            _ => 0x0000,
        }
    }

    /// True for the Xtensa parts; the rest are RISC-V.
    pub fn is_xtensa(self) -> bool {
        matches!(self, Chip::Esp32 | Chip::Esp32S2 | Chip::Esp32S3)
    }

    /// QEMU system binary that can run this chip, if one exists at all.
    pub fn qemu_binary(self) -> Option<&'static str> {
        match self {
            Chip::Esp32 | Chip::Esp32S3 => Some("qemu-system-xtensa"),
            Chip::Esp32C3 => Some("qemu-system-riscv32"),
            _ => None,
        }
    }

    /// QEMU `-M` machine name in Espressif's fork.
    pub fn qemu_machine(self) -> Option<&'static str> {
        match self {
            Chip::Esp32 => Some("esp32"),
            Chip::Esp32S3 => Some("esp32s3"),
            Chip::Esp32C3 => Some("esp32c3"),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Chip::Esp32 => "ESP32",
            Chip::Esp32S2 => "ESP32-S2",
            Chip::Esp32S3 => "ESP32-S3",
            Chip::Esp32C2 => "ESP32-C2",
            Chip::Esp32C3 => "ESP32-C3",
            Chip::Esp32C5 => "ESP32-C5",
            Chip::Esp32C6 => "ESP32-C6",
            Chip::Esp32H2 => "ESP32-H2",
            Chip::Esp32P4 => "ESP32-P4",
        }
    }
}

impl fmt::Display for Chip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Every chip target, for enumerating in UI and error messages.
pub const ALL_CHIPS: &[Chip] = &[
    Chip::Esp32,
    Chip::Esp32S2,
    Chip::Esp32S3,
    Chip::Esp32C2,
    Chip::Esp32C3,
    Chip::Esp32C5,
    Chip::Esp32C6,
    Chip::Esp32H2,
    Chip::Esp32P4,
];

impl std::str::FromStr for Chip {
    type Err = ();

    /// Accepts the spellings people actually write: `esp32s3`, `esp32-s3`,
    /// `ESP32_S3`, `s3`.
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        let norm: String = s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let norm = norm.strip_prefix("esp32").unwrap_or(&norm);
        Ok(match norm {
            "" => Chip::Esp32,
            "s2" => Chip::Esp32S2,
            "s3" => Chip::Esp32S3,
            "c2" => Chip::Esp32C2,
            "c3" => Chip::Esp32C3,
            "c5" => Chip::Esp32C5,
            "c6" => Chip::Esp32C6,
            "h2" => Chip::Esp32H2,
            "p4" => Chip::Esp32P4,
            _ => return Err(()),
        })
    }
}

/// The `spi_size` nibble of the image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashSize(pub u32);

impl FlashSize {
    pub const MB16: FlashSize = FlashSize(16 * 1024 * 1024);

    pub fn from_header_nibble(nibble: u8) -> Option<Self> {
        let mb = match nibble {
            0 => 1u32,
            1 => 2,
            2 => 4,
            3 => 8,
            4 => 16,
            5 => 32,
            6 => 64,
            7 => 128,
            _ => return None,
        };
        Some(FlashSize(mb * 1024 * 1024))
    }

    pub fn bytes(self) -> u32 {
        self.0
    }

    pub fn megabytes(self) -> u32 {
        self.0 / (1024 * 1024)
    }
}

impl fmt::Display for FlashSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}MB", self.megabytes())
    }
}

impl std::str::FromStr for FlashSize {
    type Err = ();

    /// Accepts `16MB`, `16M`, `16 MiB`, `8mb`, or a raw byte count.
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        let s = s.trim().to_ascii_lowercase();
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return Err(());
        }
        let n: u32 = digits.parse().map_err(|_| ())?;
        let unit: String = s[digits.len()..].chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = match unit.as_str() {
            "" => n,
            "k" | "kb" | "kib" => n.checked_mul(1024).ok_or(())?,
            "m" | "mb" | "mib" => n.checked_mul(1024 * 1024).ok_or(())?,
            _ => return Err(()),
        };
        if bytes == 0 {
            return Err(());
        }
        Ok(FlashSize(bytes))
    }
}
