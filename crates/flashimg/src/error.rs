use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Buffer ended before a structure we were mid-way through reading.
    Truncated { what: &'static str, need: usize, got: usize },
    /// The 0xE9 magic byte at the start of an app image was wrong.
    BadImageMagic(u8),
    /// `chip_id` held a value we don't have a mapping for.
    UnknownChip(u16),
    /// Partition table entry magic was neither an entry nor a terminator.
    BadPartitionMagic(u16),
    /// A partition extended past the end of the flash device.
    PartitionOutOfBounds { label: String, end: u64, flash: u64 },
    /// Two partitions claim the same flash bytes.
    PartitionOverlap { a: String, b: String },
    /// Content did not fit the region it was assigned.
    RegionTooSmall { what: String, need: usize, have: usize },
    /// A patch could not be placed: no such symbol, or it does not land
    /// anywhere the image actually maps.
    Unpatchable(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated { what, need, got } => {
                write!(f, "truncated {what}: need {need} bytes, got {got}")
            }
            Error::BadImageMagic(b) => {
                write!(f, "not an ESP app image: expected magic 0xE9, found {b:#04x}")
            }
            Error::UnknownChip(id) => write!(f, "unknown chip id {id:#06x}"),
            Error::Unpatchable(why) => write!(f, "cannot patch: {why}"),
            Error::BadPartitionMagic(m) => {
                write!(f, "bad partition entry magic {m:#06x}")
            }
            Error::PartitionOutOfBounds { label, end, flash } => write!(
                f,
                "partition {label:?} ends at {end:#x}, past the {flash:#x}-byte flash"
            ),
            Error::PartitionOverlap { a, b } => {
                write!(f, "partitions {a:?} and {b:?} overlap")
            }
            Error::RegionTooSmall { what, need, have } => {
                write!(f, "{what} needs {need} bytes but only {have} are available")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Little-endian reads that fail cleanly instead of panicking on short input.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub(crate) fn at(buf: &'a [u8], pos: usize) -> Self {
        Reader { buf, pos }
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    pub(crate) fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated {
            what,
            need: n,
            got: 0,
        })?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::Truncated {
            what,
            need: n,
            got: self.buf.len().saturating_sub(self.pos),
        })?;
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self, what: &'static str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    pub(crate) fn u16(&mut self, what: &'static str) -> Result<u16> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, what: &'static str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Decode a fixed-width NUL-padded C string field, lossily.
pub(crate) fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
