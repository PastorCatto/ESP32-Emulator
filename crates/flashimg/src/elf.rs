//! A minimal ELF32 symbol reader.
//!
//! Enough to turn an address into a function name, which serves two purposes:
//! symbolising a stuck program counter or a crash backtrace, and locating
//! `esp_wifi_*` entry points exactly when a build ships its `.elf`, instead of
//! falling back to byte-signature matching.
//!
//! Deliberately not a general ELF library. It reads the section headers, finds
//! the symbol table, and stops.

use crate::error::{c_str, Error, Reader, Result};

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS32: u8 = 1;
const ELFDATA2LSB: u8 = 1;

const SHT_SYMTAB: u32 = 2;

const STT_FUNC: u8 = 2;
const STT_OBJECT: u8 = 1;

/// `e_machine` values for the architectures we care about.
pub const EM_XTENSA: u16 = 94;
pub const EM_RISCV: u16 = 243;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Object,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub address: u32,
    pub size: u32,
    pub kind: SymbolKind,
}

impl Symbol {
    /// Does `addr` fall inside this symbol?
    ///
    /// Zero-sized symbols cover only their exact address; treating them as
    /// unbounded would let a stray label swallow everything after it.
    pub fn contains(&self, addr: u32) -> bool {
        if self.size == 0 {
            return addr == self.address;
        }
        addr >= self.address && addr < self.address + self.size
    }
}

#[derive(Debug, Clone)]
pub struct ElfSymbols {
    pub machine: u16,
    pub entry: u32,
    /// Sorted by address, so lookups can binary search.
    symbols: Vec<Symbol>,
}

impl ElfSymbols {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if !buf.starts_with(&ELF_MAGIC) {
            return Err(Error::BadImageMagic(*buf.first().unwrap_or(&0)));
        }
        // 64-bit or big-endian ELFs are not something an ESP toolchain emits,
        // and silently misreading one would be worse than refusing it.
        let class = *buf.get(4).ok_or(Error::Truncated { what: "elf ident", need: 5, got: buf.len() })?;
        let data = *buf.get(5).ok_or(Error::Truncated { what: "elf ident", need: 6, got: buf.len() })?;
        if class != ELFCLASS32 || data != ELFDATA2LSB {
            return Err(Error::UnknownChip(u16::from(class)));
        }

        let mut r = Reader::at(buf, 16);
        r.skip(2); // e_type
        let machine = r.u16("elf header")?;
        r.skip(4); // e_version
        let entry = r.u32("elf header")?;
        r.skip(4); // e_phoff
        let shoff = r.u32("elf header")? as usize;
        r.skip(4 + 2 + 2 + 2); // e_flags, e_ehsize, e_phentsize, e_phnum
        let shentsize = r.u16("elf header")? as usize;
        let shnum = r.u16("elf header")? as usize;

        let mut symbols = Vec::new();
        for i in 0..shnum {
            let off = shoff
                .checked_add(i.checked_mul(shentsize).ok_or(Error::Truncated {
                    what: "section headers",
                    need: shentsize,
                    got: 0,
                })?)
                .ok_or(Error::Truncated { what: "section headers", need: shoff, got: buf.len() })?;

            let mut s = Reader::at(buf, off);
            s.skip(4); // sh_name
            let sh_type = s.u32("section header")?;
            if sh_type != SHT_SYMTAB {
                continue;
            }
            s.skip(4 + 4); // sh_flags, sh_addr
            let sh_offset = s.u32("section header")? as usize;
            let sh_size = s.u32("section header")? as usize;
            let sh_link = s.u32("section header")? as usize;
            s.skip(4 + 4); // sh_info, sh_addralign
            let sh_entsize = s.u32("section header")?.max(1) as usize;

            // sh_link points at the string table holding this table's names.
            let str_off = section_offset(buf, shoff, shentsize, shnum, sh_link)?;
            symbols.reserve(sh_size / sh_entsize);

            for j in 0..(sh_size / sh_entsize) {
                let e = sh_offset + j * sh_entsize;
                let mut sym = Reader::at(buf, e);
                let st_name = sym.u32("symbol")? as usize;
                let address = sym.u32("symbol")?;
                let size = sym.u32("symbol")?;
                let info = sym.u8("symbol")?;

                let kind = match info & 0xf {
                    STT_FUNC => SymbolKind::Function,
                    STT_OBJECT => SymbolKind::Object,
                    _ => SymbolKind::Other,
                };
                // Unnamed and absolute-zero symbols are noise for our purposes.
                if address == 0 {
                    continue;
                }
                let name = match buf.get(str_off + st_name..) {
                    Some(tail) => c_str(&tail[..tail.len().min(512)]),
                    None => continue,
                };
                if name.is_empty() {
                    continue;
                }
                symbols.push(Symbol { name, address, size, kind });
            }
        }

        symbols.sort_by_key(|s| (s.address, s.size));
        Ok(ElfSymbols { machine, entry, symbols })
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter()
    }

    /// Exact lookup by name, for finding a known entry point.
    pub fn find(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// Which symbol contains `addr`, and how far into it the address is.
    ///
    /// Prefers a symbol whose declared size covers the address. Failing that,
    /// falls back to the nearest preceding function, which is what makes a
    /// stripped-ish table still useful for symbolising a program counter.
    pub fn resolve(&self, addr: u32) -> Option<(&Symbol, u32)> {
        // Rightmost symbol starting at or before addr.
        let idx = self.symbols.partition_point(|s| s.address <= addr);
        if idx == 0 {
            return None;
        }

        // Walk back over same-address entries looking for a real containment.
        for s in self.symbols[..idx].iter().rev().take(8) {
            if s.contains(addr) {
                return Some((s, addr - s.address));
            }
        }

        let nearest = self.symbols[..idx]
            .iter()
            .rev()
            .find(|s| s.kind == SymbolKind::Function)?;
        Some((nearest, addr - nearest.address))
    }
}

/// Offset of section `index` within the file.
fn section_offset(
    buf: &[u8],
    shoff: usize,
    shentsize: usize,
    shnum: usize,
    index: usize,
) -> Result<usize> {
    if index >= shnum {
        return Err(Error::Truncated { what: "string table section", need: index, got: shnum });
    }
    let mut s = Reader::at(buf, shoff + index * shentsize);
    s.skip(4 + 4 + 4 + 4); // sh_name, sh_type, sh_flags, sh_addr
    Ok(s.u32("section header")? as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny but structurally valid ELF32 with one symbol table.
    fn synth_elf(syms: &[(&str, u32, u32, u8)]) -> Vec<u8> {
        // Layout: header | strtab | symtab | section headers
        let mut strtab = vec![0u8];
        let mut name_offsets = Vec::new();
        for (n, _, _, _) in syms {
            name_offsets.push(strtab.len() as u32);
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
        }

        let mut symtab = Vec::new();
        for (i, (_, addr, size, info)) in syms.iter().enumerate() {
            symtab.extend_from_slice(&name_offsets[i].to_le_bytes());
            symtab.extend_from_slice(&addr.to_le_bytes());
            symtab.extend_from_slice(&size.to_le_bytes());
            symtab.push(*info);
            symtab.push(0); // st_other
            symtab.extend_from_slice(&0u16.to_le_bytes()); // st_shndx
        }

        let eh_size = 52usize;
        let strtab_off = eh_size;
        let symtab_off = strtab_off + strtab.len();
        let sh_off = symtab_off + symtab.len();

        let mut v = Vec::new();
        v.extend_from_slice(&ELF_MAGIC);
        v.push(ELFCLASS32);
        v.push(ELFDATA2LSB);
        v.resize(16, 0);
        v.extend_from_slice(&2u16.to_le_bytes()); // e_type = EXEC
        v.extend_from_slice(&EM_XTENSA.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // e_version
        v.extend_from_slice(&0x4200_0000u32.to_le_bytes()); // e_entry
        v.extend_from_slice(&0u32.to_le_bytes()); // e_phoff
        v.extend_from_slice(&(sh_off as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // e_flags
        v.extend_from_slice(&(eh_size as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
        v.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
        v.extend_from_slice(&40u16.to_le_bytes()); // e_shentsize
        v.extend_from_slice(&2u16.to_le_bytes()); // e_shnum
        v.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
        assert_eq!(v.len(), eh_size);

        v.extend_from_slice(&strtab);
        v.extend_from_slice(&symtab);

        // Section 0: the string table.
        let mut sh = |ty: u32, offset: usize, size: usize, link: u32, entsize: u32| {
            v.extend_from_slice(&0u32.to_le_bytes()); // sh_name
            v.extend_from_slice(&ty.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes()); // sh_flags
            v.extend_from_slice(&0u32.to_le_bytes()); // sh_addr
            v.extend_from_slice(&(offset as u32).to_le_bytes());
            v.extend_from_slice(&(size as u32).to_le_bytes());
            v.extend_from_slice(&link.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes()); // sh_info
            v.extend_from_slice(&4u32.to_le_bytes()); // sh_addralign
            v.extend_from_slice(&entsize.to_le_bytes());
        };
        sh(3, strtab_off, strtab.len(), 0, 0); // SHT_STRTAB
        sh(SHT_SYMTAB, symtab_off, symtab.len(), 0, 16);
        v
    }

    #[test]
    fn reads_symbols_and_machine() {
        let elf = synth_elf(&[
            ("app_main", 0x4200_1000, 0x40, STT_FUNC),
            ("some_global", 0x3fc8_0000, 4, STT_OBJECT),
        ]);
        let s = ElfSymbols::parse(&elf).expect("parse");
        assert_eq!(s.machine, EM_XTENSA);
        assert_eq!(s.entry, 0x4200_0000);
        assert_eq!(s.len(), 2);
        assert_eq!(s.find("app_main").unwrap().address, 0x4200_1000);
        assert_eq!(s.find("app_main").unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn resolves_an_address_inside_a_function() {
        let elf = synth_elf(&[("app_main", 0x4200_1000, 0x40, STT_FUNC)]);
        let s = ElfSymbols::parse(&elf).unwrap();
        let (sym, off) = s.resolve(0x4200_1024).expect("resolve");
        assert_eq!(sym.name, "app_main");
        assert_eq!(off, 0x24);
    }

    #[test]
    fn addresses_before_every_symbol_resolve_to_nothing() {
        let elf = synth_elf(&[("app_main", 0x4200_1000, 0x40, STT_FUNC)]);
        let s = ElfSymbols::parse(&elf).unwrap();
        assert!(s.resolve(0x4000_0000).is_none());
    }

    #[test]
    fn falls_back_to_the_nearest_preceding_function() {
        // Past the end of app_main's declared size, but still the best guess.
        let elf = synth_elf(&[("app_main", 0x4200_1000, 0x10, STT_FUNC)]);
        let s = ElfSymbols::parse(&elf).unwrap();
        let (sym, off) = s.resolve(0x4200_1100).expect("resolve");
        assert_eq!(sym.name, "app_main");
        assert_eq!(off, 0x100);
    }

    #[test]
    fn a_zero_sized_symbol_does_not_swallow_later_addresses() {
        let label = ("a_label", 0x4200_1000u32, 0u32, STT_OBJECT);
        let func = ("real_fn", 0x4200_2000u32, 0x20u32, STT_FUNC);
        let elf = synth_elf(&[label, func]);
        let s = ElfSymbols::parse(&elf).unwrap();
        // Between the label and the function, the label must not claim it.
        assert!(s.resolve(0x4200_1500).is_none_or(|(sym, _)| sym.name != "a_label"));
        assert_eq!(s.resolve(0x4200_2004).unwrap().0.name, "real_fn");
    }

    #[test]
    fn rejects_non_elf_and_64_bit_input() {
        assert!(ElfSymbols::parse(b"not an elf").is_err());
        let mut wrong_class = vec![0x7f, b'E', b'L', b'F', 2, 1];
        wrong_class.resize(64, 0);
        assert!(ElfSymbols::parse(&wrong_class).is_err());
    }
}
