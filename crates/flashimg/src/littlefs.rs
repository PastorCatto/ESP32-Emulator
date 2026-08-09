//! Empty LittleFS volumes.
//!
//! A LittleFS partition that was never flashed is erased, and erased flash is
//! all `0xff`. littlefs does not read that as "empty" — it reads it as a
//! corrupt dir pair and refuses to mount:
//!
//! ```text
//! esp_littlefs: lfs.c:1383:error: Corrupted dir pair at {0x0, 0x1}
//! esp_littlefs: mount failed,  (-84)
//! ```
//!
//! Firmware that passes `format_if_mount_failed` recovers on its own. Firmware
//! that does not — Bruce, the T-Deck Launcher — stays broken forever. Writing a
//! formatted superblock into the partition is what `lfs_format` would have
//! done, so the volume mounts empty and the firmware carries on.
//!
//! The layout below is littlefs v2. A metadata block is a 32-bit revision
//! count followed by commits; a commit is a run of tags, each stored
//! big-endian and XORed with the tag before it, terminated by a CRC tag. The
//! root directory lives in the dir pair at blocks 0 and 1, both holding the
//! superblock, the second one a revision newer.

/// `LFS_TYPE_SUPERBLOCK` — names the volume, and the name is literally
/// `littlefs`. Mount looks this entry up before anything else.
const TYPE_SUPERBLOCK: u32 = 0x0ff;
/// `LFS_TYPE_INLINESTRUCT` — the entry small enough to live in the tag itself,
/// which is where the geometry below goes.
const TYPE_INLINESTRUCT: u32 = 0x201;
/// `LFS_TYPE_FCRC` — checksum of the *next* program unit as it currently
/// reads. It lets a later mount tell an erased tail from a torn write, so the
/// filesystem knows whether it may append to this block.
const TYPE_FCRC: u32 = 0x5ff;
/// `LFS_TYPE_CCRC` — closes a commit.
const TYPE_CCRC: u32 = 0x500;

/// `LFS_DISK_VERSION`. Mount rejects a major mismatch and any minor newer than
/// its own, so this tracks the version the firmware here was built against.
const DISK_VERSION: u32 = 0x0002_0001;
const NAME_MAX: u32 = 255;
const FILE_MAX: u32 = 0x7fff_ffff;
const ATTR_MAX: u32 = 1022;

/// esp_littlefs erases a flash sector at a time.
pub const BLOCK_SIZE: u32 = 4096;
/// `CONFIG_LITTLEFS_WRITE_SIZE`. Only decides how far the commit pads.
pub const PROG_SIZE: usize = 128;

/// `LFS_MKTAG`: 1 bit spare, 11 bits type, 10 bits id, 10 bits length.
fn tag(kind: u32, id: u32, size: u32) -> u32 {
    ((kind & 0x7ff) << 20) | ((id & 0x3ff) << 10) | (size & 0x3ff)
}

/// Reflected CRC-32, seeded and left uninverted — littlefs chains it across a
/// commit rather than finishing it per record, so there is no final xor.
fn crc32(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}

/// One metadata commit, accumulated as it is written.
struct Commit {
    bytes: Vec<u8>,
    crc: u32,
    /// The previous tag. Every tag on disk is XORed with it, so a reader that
    /// has lost sync cannot mistake padding for a record.
    ptag: u32,
}

impl Commit {
    fn new(rev: u32) -> Self {
        let bytes = rev.to_le_bytes().to_vec();
        let crc = crc32(0xffff_ffff, &bytes);
        Self { bytes, crc, ptag: 0xffff_ffff }
    }

    fn push(&mut self, tag: u32, data: &[u8]) {
        let disk = (tag ^ self.ptag).to_be_bytes();
        self.bytes.extend_from_slice(&disk);
        self.crc = crc32(self.crc, &disk);
        self.bytes.extend_from_slice(data);
        self.crc = crc32(self.crc, data);
        self.ptag = tag;
    }

    /// Close the commit: an FCRC describing the program unit that follows, then
    /// the CCRC, whose length field covers the padding out to the end of the
    /// commit. The padding itself is not CRCed — that is what lets a reader skip
    /// it.
    fn seal(mut self, prog: usize) -> Vec<u8> {
        // Room for the CCRC is reserved before the FCRC goes in, which is why
        // this is measured here rather than after.
        let end = (self.bytes.len() + 5 * 4).div_ceil(prog) * prog;

        // The unit after this commit is erased, and erased flash is all ones.
        let mut fcrc = Vec::with_capacity(8);
        fcrc.extend_from_slice(&(prog as u32).to_le_bytes());
        fcrc.extend_from_slice(&crc32(0xffff_ffff, &vec![0xff; prog]).to_le_bytes());
        self.push(tag(TYPE_FCRC, 0x3ff, 8), &fcrc);

        let off = self.bytes.len();
        let next = end.min(off + 4 + 0x3fe);
        let pad = next - (off + 4);
        // littlefs flips the low bit of the CCRC type to keep a rewritten
        // commit from checksumming the same as the one it replaced.
        let reset = u32::from(!(next as u8) >> 7);
        let closing = tag(TYPE_CCRC + reset, 0x3ff, pad as u32);
        let disk = (closing ^ self.ptag).to_be_bytes();
        self.bytes.extend_from_slice(&disk);
        self.crc = crc32(self.crc, &disk);
        self.bytes.extend_from_slice(&self.crc.to_le_bytes());
        self.bytes.resize(end, 0xff);
        self.bytes
    }
}

/// One block of the root dir pair.
fn metadata_block(block_count: u32, rev: u32) -> Vec<u8> {
    let mut geometry = Vec::with_capacity(24);
    for field in [DISK_VERSION, BLOCK_SIZE, block_count, NAME_MAX, FILE_MAX, ATTR_MAX] {
        geometry.extend_from_slice(&field.to_le_bytes());
    }

    let mut commit = Commit::new(rev);
    commit.push(tag(TYPE_SUPERBLOCK, 0, 8), b"littlefs");
    commit.push(tag(TYPE_INLINESTRUCT, 0, geometry.len() as u32), &geometry);
    commit.seal(PROG_SIZE)
}

/// How many blocks a partition of this size holds. The superblock records it,
/// and mount rejects a volume whose count disagrees with the partition it was
/// found in, so this has to match what the firmware computes.
pub fn block_count(size: u32) -> u32 {
    size / BLOCK_SIZE
}

/// Write an empty volume over `partition`, which must be the whole partition —
/// the block count is taken from its length.
pub fn format_in_place(partition: &mut [u8]) {
    let blocks = block_count(partition.len() as u32);
    partition.fill(0xff);
    // Revisions start at 1, not 0: `lfs_format` writes the superblock, then
    // compacts onto the other block, so the pair is already one generation in.
    for (index, rev) in [(0usize, 1u32), (1, 2)] {
        let block = metadata_block(blocks, rev);
        let at = index * BLOCK_SIZE as usize;
        partition[at..at + block.len()].copy_from_slice(&block);
    }
}

/// Whether `partition` already holds a volume, so a flashed filesystem is never
/// overwritten. Only the superblock name is checked: anything else present is
/// either a real volume we must not touch or damage we cannot repair.
pub fn is_formatted(partition: &[u8]) -> bool {
    partition.len() >= 16 && &partition[8..16] == b"littlefs"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a block the way `lfs_dir_fetch` does and hand back the tags it
    /// finds. If this disagrees with littlefs the volume will not mount, so the
    /// reader is written from the on-disk rules rather than from the writer.
    fn read_back(block: &[u8]) -> Result<Vec<(u32, Vec<u8>)>, String> {
        let rev = u32::from_le_bytes(block[0..4].try_into().unwrap());
        let mut crc = crc32(0xffff_ffff, &rev.to_le_bytes());
        let mut ptag = 0xffff_ffffu32;
        let mut off = 0usize;
        let mut out = Vec::new();
        loop {
            // dsize: a tag whose length field is all ones carries no data.
            let dsize = 4 + if ptag & 0x3ff == 0x3ff { 0 } else { (ptag & 0x3ff) as usize };
            off += dsize;
            if off + 4 > block.len() {
                return Ok(out);
            }
            let disk: [u8; 4] = block[off..off + 4].try_into().unwrap();
            let tag = u32::from_be_bytes(disk) ^ ptag;
            if tag & 0x8000_0000 != 0 {
                return Ok(out); // not written yet
            }
            crc = crc32(crc, &disk);
            let size = (tag & 0x3ff) as usize;
            let body = &block[off + 4..off + 4 + size];

            if (tag & 0x7800_0000) >> 20 == TYPE_CCRC {
                let stored = u32::from_le_bytes(block[off + 4..off + 8].try_into().unwrap());
                if stored != crc {
                    return Err(format!("crc {stored:#010x} != {crc:#010x}"));
                }
            } else {
                crc = crc32(crc, body);
                out.push((tag, body.to_vec()));
            }
            ptag = tag;
        }
    }

    /// What littlefs itself writes, captured by compiling the very `lfs.c` the
    /// firmware under test was built with and running `lfs_format` over a RAM
    /// device. These are the first 64 bytes of each block of the root pair —
    /// everything after is erased.
    ///
    /// Byte equality with the real implementation is the only check that
    /// actually settles this. Reasoning about the format from its
    /// documentation produced a volume that looked right, passed a
    /// hand-written reader, and still would not mount.
    const REF_2928: [&str; 2] = [
        "01000000f00ffff76c6974746c6566732fe000100100020000100000700b0000\
         ff000000ffffff7ffe0300007feffc1080000000b3abd29a0ff0004c26ccbd2f",
        "02000000f00ffff76c6974746c6566732fe000100100020000100000700b0000\
         ff000000ffffff7ffe0300007feffc1080000000b3abd29a0ff0004cf659b8ab",
    ];
    const REF_256: [&str; 2] = [
        "01000000f00ffff76c6974746c6566732fe00010010002000010000000010000\
         ff000000ffffff7ffe0300007feffc1080000000b3abd29a0ff0004c9353c317",
        "02000000f00ffff76c6974746c6566732fe00010010002000010000000010000\
         ff000000ffffff7ffe0300007feffc1080000000b3abd29a0ff0004c43c6c693",
    ];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn expected(reference: &str) -> String {
        reference.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn matches_what_littlefs_itself_writes() {
        for (blocks, reference) in [(2928u32, REF_2928), (256, REF_256)] {
            for (index, rev) in [(0usize, 1u32), (1, 2)] {
                let block = metadata_block(blocks, rev);
                assert_eq!(
                    hex(&block[..64]),
                    expected(reference[index]),
                    "block {index} of a {blocks}-block volume"
                );
            }
        }
    }

    #[test]
    fn the_tail_of_the_commit_is_erased() {
        let block = metadata_block(2928, 1);
        assert!(block[64..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn superblock_names_the_volume() {
        let block = metadata_block(2928, 1);
        assert_eq!(&block[8..16], b"littlefs");
    }

    #[test]
    fn commit_crc_verifies() {
        // The whole point: littlefs recomputes this and rejects a mismatch.
        read_back(&metadata_block(2928, 1)).expect("crc must verify");
    }

    #[test]
    fn tags_read_back_as_written() {
        let tags = read_back(&metadata_block(2928, 1)).unwrap();
        assert_eq!(tags.len(), 3, "superblock name, inline struct, fcrc");
        assert_eq!(tags[0].0, tag(TYPE_SUPERBLOCK, 0, 8));
        assert_eq!(tags[0].1, b"littlefs");
        assert_eq!(tags[1].0, tag(TYPE_INLINESTRUCT, 0, 24));
        assert_eq!(tags[2].0, tag(TYPE_FCRC, 0x3ff, 8));
    }

    #[test]
    fn geometry_survives_the_round_trip() {
        let tags = read_back(&metadata_block(2928, 1)).unwrap();
        let geometry = &tags[1].1;
        let field = |i: usize| u32::from_le_bytes(geometry[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(field(0), DISK_VERSION);
        assert_eq!(field(1), BLOCK_SIZE);
        assert_eq!(field(2), 2928);
        assert_eq!(field(3), NAME_MAX);
    }

    #[test]
    fn commit_pads_to_a_program_unit() {
        // Short of this, the next commit would start mid-unit and the firmware
        // could not program it without rewriting what is already there.
        assert_eq!(metadata_block(2928, 1).len() % PROG_SIZE, 0);
    }

    #[test]
    fn both_blocks_of_the_pair_are_written() {
        let mut part = vec![0u8; 8 * BLOCK_SIZE as usize];
        format_in_place(&mut part);
        for block in [0usize, 1] {
            let at = block * BLOCK_SIZE as usize;
            read_back(&part[at..at + BLOCK_SIZE as usize]).expect("both must verify");
            assert_eq!(&part[at + 8..at + 16], b"littlefs");
        }
    }

    #[test]
    fn the_second_block_is_the_newer_revision() {
        let mut part = vec![0u8; 8 * BLOCK_SIZE as usize];
        format_in_place(&mut part);
        let rev = |at: usize| u32::from_le_bytes(part[at..at + 4].try_into().unwrap());
        assert_eq!(rev(0), 1);
        assert_eq!(rev(BLOCK_SIZE as usize), 2);
    }

    #[test]
    fn everything_past_the_pair_is_erased() {
        let mut part = vec![0u8; 8 * BLOCK_SIZE as usize];
        format_in_place(&mut part);
        assert!(part[2 * BLOCK_SIZE as usize..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_formatted_volume_is_recognised_and_erased_flash_is_not() {
        let mut part = vec![0xffu8; 8 * BLOCK_SIZE as usize];
        assert!(!is_formatted(&part));
        format_in_place(&mut part);
        assert!(is_formatted(&part));
    }

    #[test]
    fn block_count_matches_the_partition() {
        assert_eq!(block_count(0xb70000), 2928); // Bruce's spiffs partition
    }
}
