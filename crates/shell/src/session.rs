//! An emulator session: the loaded firmware, the assembled flash image, and
//! the QEMU process running it.
//!
//! Kept free of any UI types so the boot path can be tested on its own.

use boards::Board;
use flashimg::{Chip, Dropped, FlashImage, FlashSize};
use qemuctl::{Instance, LaunchConfig, Qemu};
use std::path::{Path, PathBuf};

/// What a dropped file was understood to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropKind {
    Firmware,
    BoardDefinition,
    SdCard,
    Unrecognised,
}

/// Classify by content where we can, extension only as a fallback. A firmware
/// image is identified by its header, not by being called `.bin`.
pub fn classify(path: &Path, head: &[u8]) -> DropKind {
    if flashimg::AppImage::looks_like_image(head) || head.starts_with(b"\x7fELF") {
        return DropKind::Firmware;
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("toml") => DropKind::BoardDefinition,
        Some("img" | "vhd" | "vhdx" | "iso") => DropKind::SdCard,
        Some("bin" | "elf") => DropKind::Firmware,
        _ => DropKind::Unrecognised,
    }
}

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Flash(flashimg::Error),
    Qemu(qemuctl::QemuError),
    /// Recognised as firmware, but not for a chip QEMU can run.
    UnsupportedChip(Chip),
    /// We could not tell what the file was.
    Unrecognised(PathBuf),
    /// A bare app image arrived with no bootloader to pair it with.
    NeedsBootloader,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "{e}"),
            SessionError::Flash(e) => write!(f, "{e}"),
            SessionError::Qemu(e) => write!(f, "{e}"),
            SessionError::UnsupportedChip(c) => {
                write!(f, "{c} firmware loaded, but QEMU cannot run that chip yet")
            }
            SessionError::Unrecognised(p) => write!(
                f,
                "{} is not an ESP firmware image, a board definition, or a disk image",
                p.display()
            ),
            SessionError::NeedsBootloader => write!(
                f,
                "this is a bare app image with no bootloader. Drop a merged image \
                 (esptool merge_bin) instead, or the project's .factory.bin"
            ),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}
impl From<flashimg::Error> for SessionError {
    fn from(e: flashimg::Error) -> Self {
        SessionError::Flash(e)
    }
}
impl From<qemuctl::QemuError> for SessionError {
    fn from(e: qemuctl::QemuError) -> Self {
        SessionError::Qemu(e)
    }
}

/// A firmware image that has been read and understood, but not yet booted.
#[derive(Debug, Clone)]
pub struct LoadedFirmware {
    pub path: PathBuf,
    pub chip: Chip,
    pub kind: &'static str,
    pub project: Option<String>,
    pub version: Option<String>,
    pub idf_version: Option<String>,
    pub size: usize,
}

/// Everything about a running (or ready-to-run) machine.
#[derive(Debug)]
pub struct Session {
    pub board: Board,
    pub firmware: Option<LoadedFirmware>,
    pub flash_path: Option<PathBuf>,
    pub instance: Option<Instance>,
    pub serial: SerialBuffer,
}

impl Session {
    pub fn new(board: Board) -> Self {
        Session {
            board,
            firmware: None,
            flash_path: None,
            instance: None,
            serial: SerialBuffer::default(),
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.instance.as_mut().is_some_and(|i| i.is_running())
    }

    /// Read and identify a firmware file, and assemble a flash image for it.
    ///
    /// Does not start anything: identifying and booting are separate so the UI
    /// can show what it found before committing to run it.
    pub fn load_firmware(&mut self, path: &Path, run_dir: &Path) -> Result<(), SessionError> {
        let raw = std::fs::read(path)?;
        let dropped = Dropped::identify(&raw);

        let chip = match dropped.chip() {
            Some(c) => c,
            None if matches!(dropped, Dropped::Elf) => {
                // ELF support needs a loader; identify it clearly rather than
                // pretending we can boot it.
                return Err(SessionError::Unrecognised(path.to_path_buf()));
            }
            None => return Err(SessionError::Unrecognised(path.to_path_buf())),
        };
        if chip.qemu_machine().is_none() {
            return Err(SessionError::UnsupportedChip(chip));
        }
        if matches!(dropped, Dropped::AppOnly(_)) {
            return Err(SessionError::NeedsBootloader);
        }

        // Prefer the board's flash size, but never smaller than the image.
        let mut size = self.board.flash_size;
        if (raw.len() as u32) > size.bytes() {
            size = FlashSize(raw.len().next_power_of_two() as u32);
        }

        let image = FlashImage::assemble(&dropped, &raw, size, None)?;
        std::fs::create_dir_all(run_dir)?;
        let flash_path = run_dir.join("flash.bin");
        std::fs::write(&flash_path, image.as_bytes())?;

        let kind = match &dropped {
            Dropped::MergedFlash(_) => "merged flash image",
            Dropped::AppOnly(_) => "application",
            Dropped::Bootloader(_) => "bootloader",
            Dropped::Elf => "ELF",
            Dropped::Unknown => "unknown",
        };
        let descriptor = match &dropped {
            Dropped::MergedFlash(m) => m.app.as_ref().and_then(|a| a.descriptor.clone()),
            Dropped::AppOnly(a) => a.descriptor.clone(),
            _ => None,
        };

        self.firmware = Some(LoadedFirmware {
            path: path.to_path_buf(),
            chip,
            kind,
            project: descriptor.as_ref().map(|d| d.project_name.clone()),
            version: descriptor.as_ref().map(|d| d.app_version.clone()),
            idf_version: descriptor.as_ref().map(|d| d.idf_version.clone()),
            size: raw.len(),
        });
        self.flash_path = Some(flash_path);
        Ok(())
    }

    /// Start the machine. Replaces any currently running one.
    pub fn boot(&mut self) -> Result<(), SessionError> {
        // Copy what we need before stopping, which needs &mut self.
        let (Some(chip), Some(flash)) = (
            self.firmware.as_ref().map(|f| f.chip),
            self.flash_path.clone(),
        ) else {
            return Ok(());
        };
        self.stop();
        self.serial.clear();

        let qemu = Qemu::locate(chip)?;
        let config = LaunchConfig::new(chip, &flash);
        self.instance = Some(Instance::spawn(&qemu, &config)?);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut inst) = self.instance.take() {
            inst.shutdown();
        }
    }

    /// Pull any new serial output into the buffer. Call once per frame.
    pub fn pump(&mut self) {
        if let Some(inst) = &mut self.instance {
            let bytes = inst.read_serial();
            if !bytes.is_empty() {
                self.serial.push(&bytes);
            }
        }
    }

    pub fn send_serial(&mut self, text: &str) {
        if let Some(inst) = &mut self.instance {
            let _ = inst.write_serial(text.as_bytes());
        }
    }
}

/// The serial console's scrollback.
#[derive(Debug, Default)]
pub struct SerialBuffer {
    text: String,
    /// Undecoded bytes, kept for the terminal's hex view. Bounded separately
    /// and much smaller, since it is only useful for recent traffic.
    raw: std::collections::VecDeque<u8>,
    /// Bytes discarded from the front, so the UI can say the log was trimmed.
    pub trimmed: usize,
}

impl SerialBuffer {
    /// Firmware can produce output indefinitely; keep the most recent slice.
    const LIMIT: usize = 512 * 1024;
    const RAW_LIMIT: usize = 16 * 1024;

    pub fn push(&mut self, bytes: &[u8]) {
        self.raw.extend(bytes.iter().copied());
        while self.raw.len() > Self::RAW_LIMIT {
            self.raw.pop_front();
        }

        // Firmware output is mostly UTF-8 but a garbled boot can emit anything,
        // so decode lossily rather than dropping the chunk.
        let chunk = String::from_utf8_lossy(bytes);
        self.text.push_str(&strip_ansi(&chunk));

        if self.text.len() > Self::LIMIT {
            let cut = self.text.len() - Self::LIMIT;
            // Never split a char boundary.
            let cut = (cut..self.text.len())
                .find(|i| self.text.is_char_boundary(*i))
                .unwrap_or(self.text.len());
            self.text.drain(..cut);
            self.trimmed += cut;
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Recent undecoded bytes, oldest first.
    pub fn raw(&self) -> Vec<u8> {
        self.raw.iter().copied().collect()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.raw.clear();
        self.trimmed = 0;
    }
}

/// Remove ANSI escape sequences.
///
/// ESP-IDF colours its log levels, and egui's text widget would otherwise show
/// the raw escape bytes.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI sequences run until a byte in 0x40..=0x7E.
        if chars.peek() == Some(&'[') {
            chars.next();
            for t in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&t) {
                    break;
                }
            }
        } else {
            // A lone escape, or a sequence we do not model; drop the next char.
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_content_not_extension() {
        // A firmware image named .dat is still firmware.
        let mut head = vec![0xe9, 1, 2, 0x2f, 0, 0, 8, 0x40, 0, 0, 0, 0];
        head.extend_from_slice(&9u16.to_le_bytes()); // chip_id = ESP32-S3
        head.extend_from_slice(&[0; 16]);
        assert_eq!(classify(Path::new("x.dat"), &head), DropKind::Firmware);
    }

    #[test]
    fn classifies_by_extension_when_content_is_inconclusive() {
        assert_eq!(classify(Path::new("b.toml"), b"[board]"), DropKind::BoardDefinition);
        assert_eq!(classify(Path::new("c.img"), b"\0\0\0\0"), DropKind::SdCard);
        assert_eq!(classify(Path::new("d.vhd"), b"\0\0\0\0"), DropKind::SdCard);
        assert_eq!(classify(Path::new("e.txt"), b"hello"), DropKind::Unrecognised);
    }

    #[test]
    fn elf_is_recognised_as_firmware() {
        assert_eq!(classify(Path::new("f.elf"), b"\x7fELF\x02\x01"), DropKind::Firmware);
    }

    #[test]
    fn serial_buffer_strips_ansi_colour() {
        let mut b = SerialBuffer::default();
        b.push(b"\x1b[0;32mI (123) boot: ok\x1b[0m\n");
        assert_eq!(b.text(), "I (123) boot: ok\n");
    }

    #[test]
    fn serial_buffer_survives_invalid_utf8() {
        let mut b = SerialBuffer::default();
        b.push(&[0xff, 0xfe, b'o', b'k']);
        assert!(b.text().ends_with("ok"), "got {:?}", b.text());
    }

    #[test]
    fn serial_buffer_trims_without_splitting_characters() {
        let mut b = SerialBuffer::default();
        // Multi-byte characters straddling the cut point must not panic.
        for _ in 0..40_000 {
            b.push("ünïcødé line\n".as_bytes());
        }
        assert!(b.trimmed > 0, "buffer should have been trimmed");
        assert!(b.text().len() <= SerialBuffer::LIMIT);
    }
}
