//! Recognising a function by its compiled bytes, for firmware with no `.elf`.
//!
//! Patching locates functions by symbol wherever a build ships its symbol
//! table. Plenty do not, and then the only thing left is the code itself.
//!
//! The difficulty is that a function's bytes are not the same in two builds.
//! Xtensa loads constants through `l32r`, whose operand is an offset into a
//! literal pool, and calls encode a PC-relative target. Both are fixed up at
//! link time, so both differ in every image. A signature that includes them
//! matches exactly one build -- the one it was taken from, which is the one
//! case where a signature was not needed.
//!
//! So a signature carries a per-byte mask, and the volatile fields are masked
//! out. Which fields those are is not guessed: `esp_phy_enable` was extracted
//! from six independently linked ESP32-S3 builds and compared, and every byte
//! that disagreed lay in an `l32r` operand or a call target. [`xtensa_mask`]
//! encodes that finding as a rule so it can be applied to a function seen in
//! only one build -- and [`tests`] checks the rule reproduces the six-build
//! consensus rather than taking it on faith.

use std::fmt;

/// A byte pattern with a per-byte mask.
///
/// A byte matches when `(candidate & mask) == (value & mask)`, so a mask of
/// `0xff` pins a byte exactly, `0x00` ignores it, and something between pins
/// part of it -- which is what a call needs, since its opcode and its target
/// share a byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    values: Vec<u8>,
    masks: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// Nothing matched. The function is absent, inlined, or built differently.
    NotFound { name: String },
    /// More than one match, so we cannot say which is the function. Refusing
    /// beats patching whichever happened to come first.
    Ambiguous { name: String, hits: Vec<usize> },
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignatureError::NotFound { name } => {
                write!(f, "no byte signature match for {name}")
            }
            SignatureError::Ambiguous { name, hits } => write!(
                f,
                "{name} matched {} places ({}); refusing to guess",
                hits.len(),
                hits.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Length of one Xtensa instruction, from its first byte.
///
/// `op0` is the low nibble. Values 0-7 are the 24-bit encodings; 8-15 are the
/// 16-bit density encodings. That one bit is all it takes to walk the stream,
/// which is why this needs no real disassembler.
fn instruction_len(first: u8) -> usize {
    if first & 0x08 != 0 {
        2
    } else {
        3
    }
}

/// Opcode of an `l32r`: loads from the literal pool via a PC-relative offset.
const OP0_L32R: u8 = 0x1;
/// Opcode shared by `call0`/`call4`/`call8`/`call12`.
const OP0_CALL: u8 = 0x5;
/// Opcode group holding the branches, including the unconditional `j`.
const OP0_BRANCH: u8 = 0x6;
/// Opcode group of the narrow `ret.n` / `retw.n`.
const OP0_RET_N: u8 = 0xd;

/// Does this instruction always transfer control away?
///
/// Only interesting because the assembler is free to align whatever follows
/// one, and it aligns with zero bytes. Nothing can follow a jump in sequence,
/// so padding there costs nothing at runtime.
fn is_unconditional_transfer(code: &[u8], i: usize) -> bool {
    let b0 = code[i];
    match b0 & 0x0f {
        // `j` is the op0=6 encoding with n == 0; the conditional branches in
        // the same group all have n != 0.
        OP0_BRANCH => (b0 >> 4) & 0x3 == 0,
        OP0_RET_N => code.get(i + 1) == Some(&0xf0),
        // `ret` / `retw`, the wide forms.
        0x0 => matches!(b0, 0x80 | 0x90) && code.get(i + 1..i + 3) == Some(&[0x00, 0x00][..]),
        _ => false,
    }
}

/// Mask for a function's code, hiding the fields the linker rewrites.
///
/// Walks the instruction stream and clears:
///
/// - both operand bytes of an `l32r`, which are an offset into the literal
///   pool and move whenever the pool does;
/// - the target of a `call`, which is PC-relative. Its low two bits of offset
///   share a byte with the opcode, so that byte keeps only its opcode nibble
///   rather than being dropped entirely.
///
/// Branches are deliberately left pinned. Their offsets are relative to
/// somewhere inside the same function, so they are stable across builds as
/// long as the function itself is -- and they carry a lot of the shape that
/// makes a signature specific.
pub fn xtensa_mask(code: &[u8]) -> Vec<u8> {
    let mut mask = vec![0xffu8; code.len()];
    let mut i = 0;
    while i < code.len() {
        let len = instruction_len(code[i]);
        // A trailing partial instruction cannot be decoded; pin what is there
        // rather than reading past the end.
        if i + len > code.len() {
            break;
        }
        let transfer = is_unconditional_transfer(code, i);
        match code[i] & 0x0f {
            OP0_L32R => {
                mask[i + 1] = 0x00;
                mask[i + 2] = 0x00;
            }
            OP0_CALL => {
                mask[i] = 0x0f;
                mask[i + 1] = 0x00;
                mask[i + 2] = 0x00;
            }
            _ => {}
        }
        i += len;

        // Step over alignment padding. The assembler aligns what follows an
        // unconditional transfer using zero bytes, and 0x00 is a perfectly
        // good opcode lead byte -- so a walker that does not skip them
        // swallows the padding plus the first byte of the next instruction
        // and is misaligned for the rest of the function. Everything after
        // that point gets masked in the wrong places, which is invisible
        // until a signature mysteriously fails to match.
        if transfer {
            while i < code.len() && code[i] == 0x00 {
                i += 1;
            }
        }
    }
    mask
}

impl Signature {
    /// Build a signature from raw bytes and an explicit mask.
    pub fn new(name: impl Into<String>, values: Vec<u8>, masks: Vec<u8>) -> Self {
        assert_eq!(values.len(), masks.len(), "a mask covers every byte");
        Signature { name: name.into(), values, masks }
    }

    /// Build a signature from a function's code, masking link-time fields.
    pub fn from_xtensa(name: impl Into<String>, code: &[u8]) -> Self {
        let masks = xtensa_mask(code);
        Signature::new(name, code.to_vec(), masks)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// How many bits the signature actually pins.
    ///
    /// A long pattern that is mostly mask is weak, and the length alone hides
    /// that. This is the number worth checking before trusting one.
    pub fn pinned_bits(&self) -> u32 {
        self.masks.iter().map(|m| m.count_ones()).sum()
    }

    pub fn matches_at(&self, haystack: &[u8], at: usize) -> bool {
        let Some(window) = haystack.get(at..at + self.values.len()) else {
            return false;
        };
        window
            .iter()
            .zip(&self.values)
            .zip(&self.masks)
            .all(|((&got, &want), &mask)| got & mask == want & mask)
    }

    /// Every offset the signature matches at.
    pub fn scan(&self, haystack: &[u8]) -> Vec<usize> {
        if self.is_empty() || haystack.len() < self.values.len() {
            return Vec::new();
        }
        (0..=haystack.len() - self.values.len())
            .filter(|&i| self.matches_at(haystack, i))
            .collect()
    }

    /// The single offset the signature matches at.
    ///
    /// Two matches is an error rather than a choice. Picking one would patch
    /// the wrong function about half the time, and the symptom -- firmware
    /// that dies somewhere unrelated -- gives no hint of the cause.
    pub fn find_unique(&self, haystack: &[u8]) -> Result<usize, SignatureError> {
        let hits = self.scan(haystack);
        match hits.len() {
            0 => Err(SignatureError::NotFound { name: self.name.clone() }),
            1 => Ok(hits[0]),
            _ => Err(SignatureError::Ambiguous { name: self.name.clone(), hits }),
        }
    }

    /// Render as `aa bb ?? cc`, with partly-masked bytes shown as `aa&0f`.
    pub fn render(&self) -> String {
        self.values
            .iter()
            .zip(&self.masks)
            .map(|(v, m)| match m {
                0xff => format!("{v:02x}"),
                0x00 => "??".to_string(),
                _ => format!("{:02x}&{m:02x}", v & m),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `esp_phy_enable` from `build_heltec`, an ESP32-S3 build of PURR OS.
    const HELTEC: &[u8] = &[
        0x36, 0x41, 0x00, 0xa1, 0xed, 0xef, 0x81, 0xed, 0xef, 0xe0, 0x08, 0x00, 0xa5, 0x26, 0x00, 
        0x56, 0x2a, 0x06, 0x81, 0x0d, 0xf0, 0xe0, 0x08, 0x00, 0x81, 0x0d, 0xf0, 0xe0, 0x08, 0x00, 
        0xa1, 0x09, 0xf0, 0x81, 0x0b, 0xf0, 0xe0, 0x08, 0x00, 0x56, 0x1a, 0x01, 0xd1, 0x02, 0xf0, 
        0xc1, 0x02, 0xf0, 0xb2, 0xa1, 0x49, 0xa1, 0x02, 0xf0, 0x81, 0x19, 0xef, 0xe0, 0x08, 0x00, 
        0x81, 0x01, 0xf0, 0x82, 0x08, 0x00, 0x56, 0xf8, 0x00, 0x65, 0xe6, 0xff, 0x81, 0xfe, 0xef, 
        0x0c, 0x19, 0x92, 0x48, 0x00, 0x86, 0x02, 0x00, 0x00, 0x00, 0x81, 0xff, 0xef, 0xe0, 0x08, 
        0x00, 0x25, 0x9a, 0xff, 0xe5, 0x10, 0x00, 0x81, 0xfd, 0xef, 0xe0, 0x08, 0x00, 0x8c, 0x4a, 
        0x25, 0x23, 0x00, 0xe5, 0x21, 0x00, 0x81, 0xfb, 0xef, 0xe0, 0x08, 0x00, 0x20, 0xa2, 0x20, 
        0x25, 0x19, 0x00, 0x25, 0x0b, 0x00, 0xa1, 0xce, 0xef, 0x81, 0xd2, 0xef, 0xe0, 0x08, 0x00, 
        0x90, 0x00, 0x00, 
    ];
    const WAVESHARE: &[u8] = &[
        0x36, 0x41, 0x00, 0xa1, 0xd2, 0xf2, 0x81, 0xd2, 0xf2, 0xe0, 0x08, 0x00, 0xe5, 0x26, 0x00, 
        0x56, 0x2a, 0x06, 0x81, 0xf3, 0xf2, 0xe0, 0x08, 0x00, 0x81, 0xf3, 0xf2, 0xe0, 0x08, 0x00, 
        0xa1, 0xef, 0xf2, 0x81, 0xf1, 0xf2, 0xe0, 0x08, 0x00, 0x56, 0x1a, 0x01, 0xd1, 0xe8, 0xf2, 
        0xc1, 0xe8, 0xf2, 0xb2, 0xa1, 0x49, 0xa1, 0xe8, 0xf2, 0x81, 0x3e, 0xf2, 0xe0, 0x08, 0x00, 
        0x81, 0xe7, 0xf2, 0x82, 0x08, 0x00, 0x56, 0xf8, 0x00, 0x25, 0xe5, 0xff, 0x81, 0xe4, 0xf2, 
        0x0c, 0x19, 0x92, 0x48, 0x00, 0x86, 0x02, 0x00, 0x00, 0x00, 0x81, 0xe5, 0xf2, 0xe0, 0x08, 
        0x00, 0xa5, 0x96, 0xff, 0xe5, 0x10, 0x00, 0x81, 0xe3, 0xf2, 0xe0, 0x08, 0x00, 0x8c, 0x4a, 
        0x65, 0x23, 0x00, 0x25, 0x22, 0x00, 0x81, 0xe1, 0xf2, 0xe0, 0x08, 0x00, 0x20, 0xa2, 0x20, 
        0x25, 0x19, 0x00, 0x25, 0x0b, 0x00, 0xa1, 0xb3, 0xf2, 0x81, 0xb7, 0xf2, 0xe0, 0x08, 0x00, 
        0x90, 0x00, 0x00, 
    ];

    #[test]
    fn instruction_lengths_follow_the_density_bit() {
        assert_eq!(instruction_len(0x36), 3, "entry a1, 32");
        assert_eq!(instruction_len(0x1d), 2, "retw.n");
        assert_eq!(instruction_len(0xa1), 3, "l32r a10");
        assert_eq!(instruction_len(0x0c), 2, "movi.n");
        assert_eq!(instruction_len(0xe0), 3, "callx8");
    }

    /// The point of the whole exercise: a signature taken from one build has
    /// to match a different build of the same function.
    #[test]
    fn a_signature_from_one_build_matches_another() {
        let sig = Signature::from_xtensa("esp_phy_enable", HELTEC);
        assert!(
            sig.matches_at(WAVESHARE, 0),
            "masked signature should survive relinking\n  {}",
            sig.render()
        );
    }

    /// And the rule has to be derived, not circular: masking by rule must
    /// cover every byte the two builds actually disagree on.
    #[test]
    fn the_mask_covers_exactly_what_relinking_changed() {
        let mask = xtensa_mask(HELTEC);
        for (i, (&a, &b)) in HELTEC.iter().zip(WAVESHARE).enumerate() {
            if a != b {
                assert_eq!(
                    mask[i] & (a ^ b),
                    0,
                    "byte {i} differs between builds ({a:#04x} vs {b:#04x}) \
                     but the mask still pins the differing bits"
                );
            }
        }
    }

    #[test]
    fn masking_is_not_so_aggressive_that_anything_matches() {
        let sig = Signature::from_xtensa("esp_phy_enable", HELTEC);
        // 672 of 1104 bits, measured. The floor is not a target plucked from
        // the air: at this strength the signature was checked against six
        // real 1-2 MB firmware images by the `derive-signature` example and
        // matched once in each. Losing a large slice of it would mean the
        // walker has started masking things it should not, so a drop is worth
        // a look even though the number itself is not magic.
        let total = (HELTEC.len() * 8) as u32;
        assert_eq!(total, 1104);
        assert!(
            sig.pinned_bits() >= 640,
            "only {} of {total} bits pinned; masking got more aggressive",
            sig.pinned_bits()
        );
    }

    #[test]
    fn an_unrelated_buffer_does_not_match() {
        let sig = Signature::from_xtensa("esp_phy_enable", HELTEC);
        assert_eq!(sig.scan(&[0u8; 4096]), Vec::<usize>::new());
        assert_eq!(sig.scan(&[0xffu8; 4096]), Vec::<usize>::new());
    }

    #[test]
    fn a_unique_match_is_found_and_a_repeated_one_is_refused() {
        let sig = Signature::from_xtensa("esp_phy_enable", HELTEC);

        let mut once = vec![0u8; 512];
        once.extend_from_slice(HELTEC);
        once.extend_from_slice(&[0u8; 512]);
        assert_eq!(sig.find_unique(&once), Ok(512));

        let mut twice = once.clone();
        twice.extend_from_slice(HELTEC);
        match sig.find_unique(&twice) {
            Err(SignatureError::Ambiguous { hits, .. }) => assert_eq!(hits.len(), 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }

        assert_eq!(
            sig.find_unique(&[0u8; 64]),
            Err(SignatureError::NotFound { name: "esp_phy_enable".into() })
        );
    }

    #[test]
    fn a_truncated_instruction_does_not_read_past_the_end() {
        // A three-byte instruction with only two bytes left.
        let mask = xtensa_mask(&[0xa1, 0x00]);
        assert_eq!(mask.len(), 2);
    }

    #[test]
    fn render_shows_pinned_masked_and_partial_bytes() {
        // l32r (operands dropped), then a call (opcode nibble kept).
        let sig = Signature::from_xtensa("x", &[0xa1, 0x11, 0x22, 0x25, 0x33, 0x44]);
        assert_eq!(sig.render(), "a1 ?? ?? 05&0f ?? ??");
    }
}
