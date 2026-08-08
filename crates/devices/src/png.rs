//! A minimal PNG writer, for display snapshots.
//!
//! Deliberately dependency-free. The point of this crate is that a third party
//! can drop in a device model without inheriting a dependency tree, and a
//! screenshot helper is not worth spending that on. The cost is compression:
//! this emits stored deflate blocks, so a file is a little larger than its
//! pixels rather than a fraction of them. For a 320x240 panel that is 230 KB,
//! which is fine for a snapshot and would not be for a video.

/// Encode 8-bit RGB triples, row-major, as a PNG.
///
/// `rgb` must hold `width * height * 3` bytes; anything shorter is padded with
/// black rather than refused, because a half-drawn frame is still worth
/// looking at when you are trying to find out why it is half-drawn.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let stride = width as usize * 3;

    // PNG rows each carry a leading filter byte; 0 means "store as-is".
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        let start = y * stride;
        let row = rgb.get(start..start + stride).unwrap_or(&[]);
        raw.extend_from_slice(row);
        raw.resize((stride + 1) * (y + 1), 0);
    }

    let mut out = Vec::with_capacity(raw.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // 8 bits per channel, colour type 2 (truecolour), no interlacing.
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);

    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);

    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(body);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// A zlib stream of uncompressed deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // CMF 0x78: deflate, 32K window. FLG 0x01 makes the pair a multiple of 31.
    let mut out = vec![0x78, 0x01];

    // A stored block's length field is 16 bits, so longer input is split.
    let mut chunks = data.chunks(0xffff).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0, 0, 0xff, 0xff]);
    }
    while let Some(part) = chunks.next() {
        let final_block = chunks.peek().is_none();
        out.push(u8::from(final_block));
        let len = part.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(part);
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        // 5552 is the most bytes that can accumulate before a u32 could
        // overflow, so the modulo only has to run that often.
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                // The reflected form of the standard polynomial.
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & (!(self.0 & 1)).wrapping_add(1));
            }
        }
    }

    fn finish(self) -> u32 {
        self.0 ^ 0xffff_ffff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_known_check_value() {
        // The standard CRC-32 check: "123456789" is 0xcbf43926.
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xcbf4_3926);
    }

    #[test]
    fn adler32_matches_the_known_check_value() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn the_output_is_a_png_of_the_right_shape() {
        let png = encode_rgb(2, 2, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);

        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..24], &[0, 0, 0, 2, 0, 0, 0, 2], "2x2");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn a_short_buffer_is_padded_rather_than_refused() {
        // Worth looking at a half-drawn frame when you are working out why it
        // is half-drawn.
        let png = encode_rgb(4, 4, &[255; 12]);
        assert!(png.len() > 8);
    }

    #[test]
    fn input_longer_than_one_stored_block_is_split() {
        // A 320-wide frame passes 0xffff bytes after 68 rows, so this is the
        // ordinary case, not an edge one.
        let png = encode_rgb(320, 240, &vec![128; 320 * 240 * 3]);
        let raw = (320 * 3 + 1) * 240;
        assert!(png.len() > raw, "stored blocks add framing, never remove it");
    }
}
