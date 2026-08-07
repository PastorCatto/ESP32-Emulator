//! Replacing functions in a built image.
//!
//! Some hardware cannot be emulated at all. The ESP32's radio is the case
//! that forced this: the PHY is a closed blob calibrating analog circuitry
//! that does not exist here, and the MAC below it is undocumented. Modelling
//! registers gets the blob part-way and then it waits forever for a radio.
//!
//! So the radio is bypassed instead of emulated -- the emulator answers the
//! driver's questions rather than pretending to be the silicon underneath it.
//! That is a different kind of fidelity, and it is worth being clear about
//! which one you are getting: firmware patched this way is not running the
//! code it would run on hardware.
//!
//! Patches are located by symbol, which means an ELF for the exact build.
//! Nothing here guesses: a symbol that is missing, or an address that does
//! not land inside a mapped segment, is an error rather than a patch applied
//! somewhere unfortunate.

use crate::elf::ElfSymbols;
use crate::image::AppImage;
use crate::{Error, Result};

/// Where a virtual address lives in the flash image.
///
/// Only flash-mapped segments can be patched. A segment loaded into RAM at
/// boot could be patched on flash too, but the bootloader verifies the image
/// checksum before copying it, so the change would be rejected -- see
/// [`Patcher::apply`].
fn flash_offset(image: &AppImage, app_offset: usize, addr: u32) -> Option<usize> {
    image.segments.iter().find_map(|seg| {
        let end = seg.load_addr.checked_add(seg.len)?;
        (addr >= seg.load_addr && addr < end).then(|| {
            app_offset + seg.file_offset + (addr - seg.load_addr) as usize
        })
    })
}

/// Xtensa instruction bytes, taken from real firmware rather than assembled
/// here -- the encodings were read out of a disassembly of `esp_wifi_start`
/// in a build for this chip, which is the one way to be sure they are right
/// without shipping an assembler.
mod xtensa {
    /// `entry a1, 32` — opens a stack frame in the windowed ABI. A function
    /// entered with `call8` must execute one before it returns.
    pub const ENTRY_A1_32: [u8; 3] = [0x36, 0x41, 0x00];
    /// `retw.n` — return through the window.
    pub const RETW_N: [u8; 2] = [0x1d, 0xf0];

    /// `movi.n a2, imm` — a2 is the return value in this ABI.
    ///
    /// `imm[6:4] << 12 | reg << 8 | imm[3:0] << 4 | 0xC`.
    ///
    /// Non-negative only, and that restriction is load-bearing. MOVI.N shares
    /// its opcode with BNEZ.N, which claims the encodings where the top bits
    /// are set -- exactly where a negative immediate would land. Assembling
    /// `movi.n a2, -1` this way produces `bnez.n a2, +62`, which does not
    /// return a value at all: it branches back into the middle of the
    /// function being replaced. Verified by disassembling a patched image.
    ///
    /// Nothing needs negatives. Callers test `!= ESP_OK`, so any non-zero
    /// value reports failure.
    pub fn movi_n_a2(imm: u8) -> [u8; 2] {
        assert!(imm <= 95, "movi.n cannot encode {imm}");
        let imm = u16::from(imm);
        let word = ((imm >> 4) << 12) | (2 << 8) | ((imm & 0xf) << 4) | 0xc;
        word.to_le_bytes()
    }
}

/// What to replace a function with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stub {
    /// Return immediately, doing nothing. For `void` functions.
    ReturnVoid,
    /// Return a constant. `esp_err_t` is a signed int in a2, so `0` is
    /// `ESP_OK`, and callers treat any other value as a failure.
    ReturnConst(u8),
}

impl Stub {
    fn bytes(self) -> Vec<u8> {
        let mut v = xtensa::ENTRY_A1_32.to_vec();
        if let Stub::ReturnConst(imm) = self {
            v.extend_from_slice(&xtensa::movi_n_a2(imm));
        }
        v.extend_from_slice(&xtensa::RETW_N);
        v
    }
}

/// One function to replace.
#[derive(Debug, Clone)]
pub struct Patch {
    pub symbol: String,
    pub stub: Stub,
    /// Why, for the log. A patched image should be able to explain itself.
    pub reason: &'static str,
}

/// Everything needed to boot firmware whose radio cannot be emulated.
///
/// `esp_phy_enable` is the one that matters: without it a boot stops there
/// forever. The rest keep the Wi-Fi driver's own state machine consistent
/// afterwards, so firmware believes the interface came up and carries on.
///
/// Shared by the UI and the command-line tool deliberately -- two lists that
/// drift apart would mean the tool and the application patch differently, and
/// only one of them would be the one you tested.
pub fn radio_bypass() -> Vec<Patch> {
    let ok = |name: &str, reason| Patch {
        symbol: name.into(),
        stub: Stub::ReturnConst(0),
        reason,
    };
    vec![
        Patch {
            symbol: "esp_phy_enable".into(),
            stub: Stub::ReturnVoid,
            reason: "calibrates a radio that does not exist; never returns",
        },
        ok("esp_wifi_init", "would start the MAC"),
        ok("esp_wifi_set_mode", "driver state only"),
        ok("esp_wifi_set_config", "driver state only"),
        ok("esp_wifi_start", "would bring the MAC up"),
        ok("esp_wifi_stop", "nothing to stop"),
        ok("esp_wifi_disconnect", "nothing to disconnect"),
        ok("esp_wifi_connect", "no radio to associate with"),
        // Reports failure rather than an empty result: an empty scan is
        // indistinguishable from a real scan of an empty room, and it would
        // also send the caller into an uninitialised result buffer.
        Patch {
            symbol: "esp_wifi_scan_start".into(),
            stub: Stub::ReturnConst(1),
            reason: "no radio to hear beacons; reports failure",
        },
    ]
}

/// A patch that was applied, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub symbol: String,
    pub address: u32,
    pub offset: usize,
    pub bytes: usize,
    /// Whether the address came from the symbol table or from recognising
    /// the bytes. Worth surfacing: one is exact, the other is inference.
    pub how: Located,
}

#[derive(Debug)]
pub struct Patcher<'a> {
    /// Absent when the firmware shipped no `.elf`, which is the case byte
    /// signatures exist for.
    symbols: Option<&'a ElfSymbols>,
    image: AppImage,
    /// Where the app image starts within the flash image.
    app_offset: usize,
}

/// How a function was located, which the caller should show rather than hide.
///
/// A symbol is exact. A signature is a considered guess: it says the bytes
/// look like a function we recognise from a build we could check, which is
/// strong evidence but not proof, and the two deserve different confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    Symbol,
    Signature,
}

impl<'a> Patcher<'a> {
    /// `app_offset` is where the application image begins in flash, which the
    /// partition table decides -- 0x10000 in every default layout.
    pub fn new(symbols: &'a ElfSymbols, flash: &[u8], app_offset: usize) -> Result<Self> {
        let image = AppImage::parse(&flash[app_offset..])?;
        Ok(Patcher { symbols: Some(symbols), image, app_offset })
    }

    /// A patcher for firmware with no symbol table, which must fall back to
    /// byte signatures for every function it touches.
    pub fn from_signatures(flash: &[u8], app_offset: usize) -> Result<Self> {
        let image = AppImage::parse(&flash[app_offset..])?;
        Ok(Patcher { symbols: None, image, app_offset })
    }

    /// Turn a flash offset back into the address the code runs at.
    fn vaddr_at(&self, offset: usize) -> Option<u32> {
        let within = offset.checked_sub(self.app_offset)?;
        self.image.segments.iter().find_map(|seg| {
            let start = seg.file_offset;
            let end = start.checked_add(seg.len as usize)?;
            (within >= start && within < end)
                .then(|| seg.load_addr + (within - start) as u32)
        })
    }

    /// Where a patch should be written, and how sure we are of it.
    ///
    /// Symbols first, always: they are exact and free. Signatures only when
    /// there is no symbol table at all -- not as a cross-check on a build
    /// that has one, since a disagreement there would mean the signature is
    /// wrong and the symbol is right.
    fn locate(&self, patch: &Patch, flash: &[u8]) -> Result<(usize, u32, usize, Located)> {
        if let Some(symbols) = self.symbols {
            let symbol = symbols.find(&patch.symbol).ok_or_else(|| {
                Error::Unpatchable(format!("no symbol {:?} in the ELF", patch.symbol))
            })?;
            let offset = flash_offset(&self.image, self.app_offset, symbol.address)
                .ok_or_else(|| {
                    Error::Unpatchable(format!(
                        "{} is at {:#010x}, which is not in a flash-mapped segment",
                        patch.symbol, symbol.address
                    ))
                })?;
            return Ok((offset, symbol.address, symbol.size as usize, Located::Symbol));
        }

        let signature = crate::signatures::find(&patch.symbol).ok_or_else(|| {
            Error::Unpatchable(format!(
                "{} has no byte signature, and the firmware shipped no .elf",
                patch.symbol
            ))
        })?;
        // Search the app image only. The rest of flash holds the bootloader,
        // NVS and whatever the filesystem contains, and a chance match out
        // there would be a patch written somewhere meaningless.
        let end = (self.app_offset + self.image.total_len).min(flash.len());
        let region = flash.get(self.app_offset..end).ok_or_else(|| {
            Error::Unpatchable("the app image runs past the end of the flash image".into())
        })?;
        let at = signature
            .find_unique(region)
            .map_err(|e| Error::Unpatchable(e.to_string()))?;
        let offset = self.app_offset + at;
        let address = self.vaddr_at(offset).unwrap_or(0);
        Ok((offset, address, signature.len(), Located::Signature))
    }

    /// Apply patches in place, then repair the image trailer.
    ///
    /// Both the XOR checksum and the appended SHA-256 have to be recomputed.
    /// The second-stage bootloader verifies them on every boot, not only
    /// under secure boot: skipping this gets
    ///
    /// ```text
    /// E esp_image: Checksum failed. Calculated 0xc1 read 0x8b
    /// E boot: Factory app partition is not bootable
    /// ```
    ///
    /// and then a boot loop.
    pub fn apply(&self, flash: &mut [u8], patches: &[Patch]) -> Result<Vec<Applied>> {
        let mut applied = Vec::with_capacity(patches.len());

        for patch in patches {
            let (offset, address, size, how) = self.locate(patch, flash)?;

            let bytes = patch.stub.bytes();
            if size < bytes.len() {
                return Err(Error::Unpatchable(format!(
                    "{} is {} bytes, too small for a {}-byte stub",
                    patch.symbol, size, bytes.len()
                )));
            }

            let end = offset + bytes.len();
            if end > flash.len() {
                return Err(Error::Unpatchable(format!(
                    "{} maps past the end of the image", patch.symbol
                )));
            }
            flash[offset..end].copy_from_slice(&bytes);

            applied.push(Applied {
                symbol: patch.symbol.clone(),
                address,
                offset,
                bytes: bytes.len(),
                how,
            });
        }

        if !applied.is_empty() {
            self.reseal(flash)?;
        }
        Ok(applied)
    }

    /// Recompute the checksum byte and, if present, the appended digest.
    fn reseal(&self, flash: &mut [u8]) -> Result<()> {
        let app = self.app_offset;
        let total = self.image.total_len;
        if app + total > flash.len() {
            return Err(Error::Unpatchable(
                "the app image runs past the end of the flash image".into(),
            ));
        }

        // The checksum covers segment payloads only, not the headers, seeded
        // with 0xEF. It sits in the last byte before the 16-byte boundary.
        let mut sum = ESP_CHECKSUM_SEED;
        for seg in &self.image.segments {
            let start = app + seg.file_offset;
            for byte in &flash[start..start + seg.len as usize] {
                sum ^= *byte;
            }
        }

        let hash_len = if self.image.header.hash_appended { 32 } else { 0 };
        let checksum_at = app + total - hash_len - 1;
        flash[checksum_at] = sum;

        if hash_len != 0 {
            // Over everything from the image header up to the digest itself.
            let digest = crate::sha256::sha256(&flash[app..app + total - 32]);
            flash[app + total - 32..app + total].copy_from_slice(&digest);
        }
        Ok(())
    }
}

/// Seed for the app image's XOR checksum, from ESP-IDF's `esp_image_format.h`.
const ESP_CHECKSUM_SEED: u8 = 0xef;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_signature_is_specific_enough_to_be_worth_having() {
        // A signature that pins only a handful of bits will match all over a
        // megabyte of firmware. The two entry points that failed this are not
        // shipped at all, so anything in the table should clear it easily.
        for sig in crate::signatures::radio_bypass() {
            let bits = (sig.len() * 8) as u32;
            assert!(
                sig.pinned_bits() >= 128,
                "{} pins only {} of {bits} bits",
                sig.name,
                sig.pinned_bits()
            );
        }
    }

    #[test]
    fn the_wrapper_entry_points_are_left_out_rather_than_guessed_at() {
        // esp_wifi_connect and esp_wifi_disconnect compile to ten bytes and
        // matched six places each. Shipping them would patch the wrong code
        // five times out of six.
        assert!(crate::signatures::find("esp_wifi_connect").is_none());
        assert!(crate::signatures::find("esp_wifi_disconnect").is_none());
        assert!(crate::signatures::find("esp_phy_enable").is_some());
    }

    #[test]
    fn movi_n_encodes_the_values_a_stub_needs() {
        // Verified by disassembling a patched image: the emulator reports
        // "movi.n a2, 0" and "movi.n a2, -1" at these bytes.
        assert_eq!(xtensa::movi_n_a2(0), [0x0c, 0x02]);
        assert_eq!(xtensa::movi_n_a2(1), [0x1c, 0x02]);
    }

    #[test]
    fn the_void_stub_is_an_entry_and_a_return() {
        // Both encodings were read out of real firmware; getting either wrong
        // turns a patched function into whatever the bytes happen to decode
        // as, which is a crash somewhere unrelated.
        assert_eq!(Stub::ReturnVoid.bytes(), vec![0x36, 0x41, 0x00, 0x1d, 0xf0]);
    }

    #[test]
    fn an_address_outside_every_segment_has_no_offset() {
        let image = AppImage {
            header: crate::image::ImageHeader {
                segment_count: 1,
                spi_mode: crate::image::SpiMode::Dio,
                spi_speed: 0,
                flash_size: None,
                entry_addr: 0,
                chip: crate::Chip::Esp32S3,
                min_chip_rev_full: 0,
                max_chip_rev_full: 0,
                hash_appended: false,
            },
            descriptor: None,
            segments: vec![crate::image::Segment {
                load_addr: 0x4200_0020,
                len: 0x1000,
                file_offset: 0x20,
            }],
            total_len: 0x2000,
        };
        assert!(flash_offset(&image, 0x10000, 0x4200_0100).is_some());
        assert_eq!(flash_offset(&image, 0x10000, 0x4300_0000), None);
    }

    #[test]
    fn a_mapped_address_lands_where_the_segment_puts_it() {
        let image = AppImage {
            header: crate::image::ImageHeader {
                segment_count: 1,
                spi_mode: crate::image::SpiMode::Dio,
                spi_speed: 0,
                flash_size: None,
                entry_addr: 0,
                chip: crate::Chip::Esp32S3,
                min_chip_rev_full: 0,
                max_chip_rev_full: 0,
                hash_appended: false,
            },
            descriptor: None,
            segments: vec![crate::image::Segment {
                load_addr: 0x4200_0020,
                len: 0x2000,
                file_offset: 0x20,
            }],
            total_len: 0x3000,
        };
        // app at 0x10000, segment payload at +0x20, 0x100 into the segment.
        assert_eq!(
            flash_offset(&image, 0x10000, 0x4200_0120),
            Some(0x10000 + 0x20 + 0x100)
        );
    }
}
