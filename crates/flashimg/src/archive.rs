//! Reading functions out of a static library, with the linker's own answer
//! about which bytes move.
//!
//! Byte signatures currently come from firmware we happen to have: extract a
//! function from a linked image and mask the fields that looked volatile. That
//! works, but it is inference twice over -- we guess which bytes the linker
//! rewrites, and we can only cover builds we own.
//!
//! Both problems have the same fix. `esp_wifi_scan_get_ap_records` and its
//! neighbours are not compiled from anyone's project: they ship precompiled in
//! Espressif's `libnet80211.a`, and Arduino and PlatformIO repackage the same
//! binaries rather than rebuilding them. So one extraction covers every
//! firmware built against that IDF version, whatever toolchain produced it.
//!
//! And an archive member is a *relocatable* object. It still carries its
//! relocation table, which is a precise list of the bytes the linker will
//! rewrite -- not a guess about them. That turns the mask from an inference
//! into a fact.

use crate::signature::{xtensa_mask, Signature};
use crate::{Error, Result};

const ARCHIVE_MAGIC: &[u8] = b"!<arch>\n";
const MEMBER_HEADER_LEN: usize = 60;

// ELF32, little endian, only the parts needed to find a function's bytes.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHT_REL: u32 = 9;
const SECTION_HEADER_LEN: usize = 40;
const SYMBOL_LEN: usize = 16;

/// Bytes masked from each relocation site.
///
/// Not the width of the relocated field: the Xtensa linker *relaxes* code,
/// rewriting `l32r`+`callx8` into a direct `call8` in place. That changes
/// instruction encodings either side of the relocation while preserving
/// length, so masking only the operand leaves pinned bytes the linker moved.
/// Measured against esp_wifi_start, where relaxation reached three bytes past
/// a four-byte window.
const RELOC_SPAN: usize = 8;

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

/// One member of an `ar` archive.
#[derive(Debug, Clone)]
pub struct Member<'a> {
    pub name: String,
    pub data: &'a [u8],
}

/// Iterate the members of an `ar` archive.
///
/// Handles the GNU long-name table, because object files produced by a real
/// build have names far longer than the 16 bytes the header allows, and
/// skipping them would silently hide most of the archive.
pub fn members(archive: &[u8]) -> Result<Vec<Member<'_>>> {
    if !archive.starts_with(ARCHIVE_MAGIC) {
        return Err(Error::Unpatchable("not an ar archive".into()));
    }

    let mut out = Vec::new();
    let mut long_names: &[u8] = &[];
    let mut at = ARCHIVE_MAGIC.len();

    while at + MEMBER_HEADER_LEN <= archive.len() {
        let header = &archive[at..at + MEMBER_HEADER_LEN];
        let raw_name = String::from_utf8_lossy(&header[0..16]).trim_end().to_string();
        let size: usize = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(|_| Error::Unpatchable("bad archive member size".into()))?;

        let start = at + MEMBER_HEADER_LEN;
        let end = start.checked_add(size).filter(|e| *e <= archive.len()).ok_or_else(|| {
            Error::Unpatchable("archive member runs past the end of the file".into())
        })?;
        let data = &archive[start..end];

        match raw_name.as_str() {
            // The symbol index and the long-name table are bookkeeping, not
            // objects.
            "/" | "" => {}
            "//" => long_names = data,
            name => {
                let resolved = if let Some(offset) = name.strip_prefix('/') {
                    // "/123" indexes the long-name table, terminated by a
                    // slash or a NUL depending on who wrote the archive.
                    offset
                        .parse::<usize>()
                        .ok()
                        .and_then(|o| long_names.get(o..))
                        .map(|rest| {
                            let end = rest
                                .iter()
                                .position(|&b| b == b'/' || b == 0)
                                .unwrap_or(rest.len());
                            String::from_utf8_lossy(&rest[..end]).into_owned()
                        })
                        .unwrap_or_else(|| name.to_string())
                } else {
                    name.trim_end_matches('/').to_string()
                };
                out.push(Member { name: resolved, data });
            }
        }

        // Members are padded to an even offset.
        at = end + (end & 1);
    }

    Ok(out)
}

/// A function lifted out of an object file.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub symbol: String,
    /// Which archive member it came from, for provenance.
    pub member: String,
    pub code: Vec<u8>,
    /// Bytes the linker rewrites, from the relocation table.
    pub relocated: Vec<bool>,
}

impl Extracted {
    /// A signature whose mask is the intersection of two independent answers.
    ///
    /// The relocation table says which bytes the linker rewrites. The
    /// instruction walker says which operand fields are link-time values. They
    /// should agree; where they do not, trusting either one alone risks
    /// pinning a byte that moves. Masking the union of both is the safe
    /// reading, and costs only a little specificity.
    pub fn signature(&self) -> Signature {
        let mut mask = xtensa_mask(&self.code);
        for (i, &moved) in self.relocated.iter().enumerate() {
            if moved {
                if let Some(m) = mask.get_mut(i) {
                    *m = 0;
                }
            }
        }
        Signature::new(self.symbol.clone(), self.code.clone(), mask)
    }
}

/// Find a function by name across every member of an archive.
pub fn find_function(archive: &[u8], symbol: &str) -> Result<Option<Extracted>> {
    for member in members(archive)? {
        if let Some(found) = function_in_object(member.data, symbol, &member.name)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Find a defined function in one ELF32 relocatable object.
fn function_in_object(obj: &[u8], symbol: &str, member: &str) -> Result<Option<Extracted>> {
    if !obj.starts_with(&ELF_MAGIC) {
        return Ok(None);
    }

    let shoff = u32_at(obj, 32).unwrap_or(0) as usize;
    let shentsize = u16_at(obj, 46).unwrap_or(0) as usize;
    let shnum = u16_at(obj, 48).unwrap_or(0) as usize;
    if shentsize < SECTION_HEADER_LEN || shnum == 0 {
        return Ok(None);
    }

    let section = |i: usize| -> Option<&[u8]> {
        let at = shoff.checked_add(i.checked_mul(shentsize)?)?;
        obj.get(at..at + SECTION_HEADER_LEN)
    };
    let body = |sh: &[u8]| -> Option<&[u8]> {
        let off = u32_at(sh, 16)? as usize;
        let len = u32_at(sh, 20)? as usize;
        obj.get(off..off.checked_add(len)?)
    };

    for i in 0..shnum {
        let Some(sh) = section(i) else { continue };
        if u32_at(sh, 4) != Some(SHT_SYMTAB) {
            continue;
        }
        let Some(symtab) = body(sh) else { continue };
        let Some(strtab) = section(u32_at(sh, 24).unwrap_or(0) as usize).and_then(body) else {
            continue;
        };

        for entry in symtab.chunks_exact(SYMBOL_LEN) {
            let name_off = u32_at(entry, 0).unwrap_or(0) as usize;
            let name = strtab
                .get(name_off..)
                .map(|r| {
                    let end = r.iter().position(|&b| b == 0).unwrap_or(r.len());
                    String::from_utf8_lossy(&r[..end]).into_owned()
                })
                .unwrap_or_default();
            if name != symbol {
                continue;
            }

            let value = u32_at(entry, 4).unwrap_or(0) as usize;
            let size = u32_at(entry, 8).unwrap_or(0) as usize;
            let shndx = u16_at(entry, 14).unwrap_or(0) as usize;
            // Undefined (shndx 0) means this object references the symbol
            // rather than defining it; keep looking.
            if shndx == 0 || shndx >= shnum || size == 0 {
                continue;
            }

            let Some(target) = section(shndx) else { continue };
            let Some(data) = body(target) else { continue };
            let Some(code) = data.get(value..value.checked_add(size).unwrap_or(0)) else {
                continue;
            };

            let mut relocated = vec![false; size];
            mark_relocations(obj, shnum, shentsize, shoff, shndx, value, size, &mut relocated);

            return Ok(Some(Extracted {
                symbol: symbol.to_string(),
                member: member.to_string(),
                code: code.to_vec(),
                relocated,
            }));
        }
    }

    Ok(None)
}

/// Mark the bytes covered by relocations against `[value, value + size)`.
///
/// Width is taken as four bytes from the relocation's offset, clamped. Xtensa
/// relocation types patch operand fields of varying width inside an
/// instruction, and over-masking merely loosens the signature a little --
/// under-masking pins a byte the linker moves, which breaks matching outright
/// and does so silently.
#[allow(clippy::too_many_arguments)]
fn mark_relocations(
    obj: &[u8],
    shnum: usize,
    shentsize: usize,
    shoff: usize,
    target: usize,
    value: usize,
    size: usize,
    out: &mut [bool],
) {
    for i in 0..shnum {
        let at = shoff + i * shentsize;
        let Some(sh) = obj.get(at..at + SECTION_HEADER_LEN) else { continue };
        let kind = u32_at(sh, 4).unwrap_or(0);
        if kind != SHT_RELA && kind != SHT_REL {
            continue;
        }
        // sh_info names the section these relocations apply to.
        if u32_at(sh, 28).unwrap_or(0) as usize != target {
            continue;
        }

        let entry_len = if kind == SHT_RELA { 12 } else { 8 };
        let off = u32_at(sh, 16).unwrap_or(0) as usize;
        let len = u32_at(sh, 20).unwrap_or(0) as usize;
        let Some(table) = obj.get(off..off.saturating_add(len)) else { continue };

        for rel in table.chunks_exact(entry_len) {
            let r_offset = u32_at(rel, 0).unwrap_or(0) as usize;
            if r_offset < value || r_offset >= value + size {
                continue;
            }
            let start = r_offset - value;
            for slot in out.iter_mut().skip(start).take(RELOC_SPAN) {
                *slot = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal archive with two named members.
    fn synth_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = ARCHIVE_MAGIC.to_vec();
        for (name, body) in entries {
            let mut header = vec![b' '; MEMBER_HEADER_LEN];
            header[..name.len()].copy_from_slice(name.as_bytes());
            let size = format!("{}", body.len());
            header[48..48 + size.len()].copy_from_slice(size.as_bytes());
            header[58] = b'`';
            header[59] = b'\n';
            out.extend_from_slice(&header);
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn reads_members_and_skips_the_bookkeeping_ones() {
        let archive = synth_archive(&[
            ("/", b"symbol index"),
            ("first.o/", b"aaaa"),
            ("second.o/", b"bbbb"),
        ]);
        let members = members(&archive).unwrap();
        assert_eq!(members.len(), 2, "the symbol index is not an object");
        assert_eq!(members[0].name, "first.o");
        assert_eq!(members[1].data, b"bbbb");
    }

    #[test]
    fn odd_sized_members_stay_aligned() {
        // A member of odd length is padded, and mis-handling that shifts every
        // later member by one byte -- which reads as a corrupt archive rather
        // than as an off-by-one.
        let archive = synth_archive(&[("a.o/", b"odd"), ("b.o/", b"next")]);
        let members = members(&archive).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[1].data, b"next");
    }

    #[test]
    fn long_member_names_come_from_the_string_table() {
        let archive = synth_archive(&[
            ("//", b"a_very_long_object_name.c.obj/\n"),
            ("/0", b"code"),
        ]);
        let members = members(&archive).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "a_very_long_object_name.c.obj");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_refused() {
        assert!(members(b"\x7fELF not an archive").is_err());
    }

    #[test]
    fn a_truncated_member_does_not_panic() {
        let mut archive = synth_archive(&[("a.o/", b"12345678")]);
        archive.truncate(archive.len() - 4);
        assert!(members(&archive).is_err());
    }

    /// The real thing, when an IDF installation is present. Skipped rather
    /// than failed elsewhere: this asserts against Espressif's shipped binary,
    /// which is exactly the point, and it cannot be vendored into the repo.
    #[test]
    fn extracts_a_wifi_function_from_espressifs_own_library() {
        let path = "C:/esp/v5.3.5/esp-idf/components/esp_wifi/lib/esp32s3/libnet80211.a";
        let Ok(archive) = std::fs::read(path) else {
            eprintln!("skipped: no IDF at {path}");
            return;
        };

        let found = find_function(&archive, "esp_wifi_scan_get_ap_records")
            .expect("archive should parse")
            .expect("esp_wifi_scan_get_ap_records is defined in libnet80211.a");

        assert!(!found.code.is_empty());
        assert_eq!(found.relocated.len(), found.code.len());
        assert!(
            found.relocated.iter().any(|&m| m),
            "a function that calls anything has relocations; none found means \
             the relocation sections were not matched to the code section"
        );

        let sig = found.signature();
        assert_eq!(sig.len(), found.code.len());
        assert!(
            sig.pinned_bits() > 0,
            "masking everything would match anywhere"
        );
        eprintln!(
            "{} from {}: {} bytes, {} pinned bits",
            found.symbol,
            found.member,
            found.code.len(),
            sig.pinned_bits()
        );
    }
}
