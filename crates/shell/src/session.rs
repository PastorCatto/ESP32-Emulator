//! An emulator session: the loaded firmware, the assembled flash image, and
//! the QEMU process running it.
//!
//! Kept free of any UI types so the boot path can be tested on its own.

use crate::hardware::{self, Hardware};
use boards::Board;
use devices::st7789::ScreenHandle;
use flashimg::{Chip, Dropped, FlashImage, FlashSize};
use qemuctl::{Instance, LaunchConfig, Qemu};
use std::path::{Path, PathBuf};

/// Where the application image starts in flash. Every default partition
/// layout puts it here.
const APP_OFFSET: usize = 0x10000;

/// What a dropped file was understood to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropKind {
    Firmware,
    /// An ELF. Not bootable, but it carries the symbols patching needs.
    Symbols,
    BoardDefinition,
    SdCard,
    Unrecognised,
}

/// Classify by content where we can, extension only as a fallback. A firmware
/// image is identified by its header, not by being called `.bin`.
pub fn classify(path: &Path, head: &[u8]) -> DropKind {
    // An ELF is never the thing we boot -- the bootloader wants a flash image
    // -- but it is the only place the symbol table lives, so it is useful in
    // its own right rather than an error.
    if head.starts_with(b"\x7fELF") {
        return DropKind::Symbols;
    }
    if flashimg::AppImage::looks_like_image(head) {
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
        Some("elf") => DropKind::Symbols,
        Some("bin") => DropKind::Firmware,
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
    /// Digest of the ELF this image was built from, as recorded by the build.
    /// `None` when the descriptor left it zeroed, which some builds do.
    pub elf_sha256: Option<[u8; 32]>,
}

/// A symbol table for the firmware, loaded from its `.elf`.
///
/// Kept separate from the firmware itself because they arrive separately: an
/// ELF is not bootable and a `.bin` carries no symbols, so patching needs
/// both and neither implies the other.
pub struct LoadedSymbols {
    pub path: PathBuf,
    pub count: usize,
    /// Digest of the ELF file itself. The build records this in the image's
    /// app descriptor, so the two can be checked against each other.
    pub sha256: [u8; 32],
    table: flashimg::ElfSymbols,
}

/// Whether a symbol table belongs to the image currently loaded.
///
/// Symbols and images arrive as separate drops and nothing stops them being
/// from different builds. A mismatched pair is worse than a missing one:
/// addresses from the wrong build usually still land *somewhere* in the
/// image, so patching succeeds and writes over whatever happened to be at
/// that offset, and the firmware fails later somewhere unrelated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolMatch {
    /// No symbols, or no image, so there is nothing to check.
    Nothing,
    /// The image records the digest of the ELF it was built from, and it is
    /// this one.
    Matches,
    /// The image names a different ELF. Patching with these is meaningless.
    Mismatch,
    /// The image carries no ELF digest, so this cannot be settled either way.
    /// Older builds, and anything not produced by `esptool elf2image`.
    Unverifiable,
}

impl std::fmt::Debug for LoadedSymbols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedSymbols")
            .field("path", &self.path)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

/// Everything about a running (or ready-to-run) machine.
#[derive(Debug)]
pub struct Session {
    pub board: Board,
    pub firmware: Option<LoadedFirmware>,
    pub flash_path: Option<PathBuf>,
    pub instance: Option<Instance>,
    pub serial: SerialLog,
    /// Disk image backing the SD card, when one has been dropped in.
    pub sd_image: Option<PathBuf>,
    /// Symbols for the loaded firmware, from a dropped `.elf`. Patching needs
    /// them: functions are located by name, never guessed at.
    pub symbols: Option<LoadedSymbols>,
    /// Patches applied to the assembled image, in the order they went in.
    /// Cleared whenever the firmware is reloaded, because that rewrites the
    /// image from its source.
    pub patches: Vec<flashimg::patch::Applied>,
    /// What was done to make the firmware's filesystem partition mountable.
    /// Worth surfacing: an empty volume is not the assets the firmware shipped
    /// with, so a missing file later on traces back to here.
    pub storage: Vec<flashimg::provision::Action>,
    /// Every serial byte, teed to a file when ESP32_SERIAL_LOG is set. Reopened
    /// per boot so a run is measurable on its own.
    serial_log: Option<std::fs::File>,
    /// The device models and the bus server, alive only while running.
    pub hardware: Option<Hardware>,
}

impl Session {
    pub fn new(board: Board) -> Self {
        Session {
            board,
            firmware: None,
            flash_path: None,
            instance: None,
            serial: SerialLog::default(),
            sd_image: None,
            symbols: None,
            patches: Vec::new(),
            storage: Vec::new(),
            serial_log: None,
            hardware: None,
        }
    }

    /// Open the serial tee for a fresh boot, if one was asked for.
    ///
    /// Truncates, so each boot's log stands alone and a timestamp in it means
    /// what it looks like it means.
    fn open_serial_log(&mut self) {
        self.serial_log = std::env::var_os("ESP32_SERIAL_LOG").and_then(|path| {
            std::fs::File::create(&path)
                .map_err(|e| eprintln!("serial log {path:?}: {e}"))
                .ok()
        });
    }

    /// The live display, if the board has a panel this build can model.
    pub fn screen(&self) -> Option<&ScreenHandle> {
        self.hardware.as_ref().and_then(|h| h.screen.as_ref())
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
        // A filesystem partition that was never flashed is erased, and no
        // firmware mounts erased flash. Give it an empty volume so the boot
        // gets past the mount instead of stopping on a black screen.
        let mut bytes = image.as_bytes().to_vec();
        let provisioned = flashimg::provision::prepare(&mut bytes);
        std::fs::create_dir_all(run_dir)?;
        let flash_path = run_dir.join("flash.bin");
        std::fs::write(&flash_path, &bytes)?;

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
            // All-zero means the build did not record one, not that the ELF
            // hashes to zero.
            elf_sha256: descriptor
                .as_ref()
                .map(|d| d.elf_sha256)
                .filter(|d| d.iter().any(|&b| b != 0)),
        });
        // The image was just rewritten from source, so any patches are gone.
        self.patches.clear();
        self.storage = provisioned;
        self.flash_path = Some(flash_path);
        Ok(())
    }

    /// Read a `.elf` for its symbols. Does not patch anything by itself.
    pub fn load_symbols(&mut self, path: &Path) -> Result<usize, SessionError> {
        let raw = std::fs::read(path)?;
        let table = flashimg::ElfSymbols::parse(&raw)?;
        let count = table.len();
        self.symbols = Some(LoadedSymbols {
            path: path.to_path_buf(),
            count,
            sha256: flashimg::sha256(&raw),
            table,
        });
        Ok(count)
    }

    /// Do the loaded symbols belong to the loaded image?
    pub fn symbol_match(&self) -> SymbolMatch {
        let (Some(symbols), Some(firmware)) = (&self.symbols, &self.firmware) else {
            return SymbolMatch::Nothing;
        };
        match firmware.elf_sha256 {
            None => SymbolMatch::Unverifiable,
            Some(recorded) if recorded == symbols.sha256 => SymbolMatch::Matches,
            Some(_) => SymbolMatch::Mismatch,
        }
    }

    /// Can the radio bypass be applied right now?
    ///
    /// Symbols from a different build are refused. They are not merely
    /// useless: the addresses usually still land inside the image, so the
    /// patch writes over something arbitrary and the firmware dies later,
    /// somewhere with no connection to the cause.
    pub fn can_patch(&self) -> bool {
        self.flash_path.is_some()
            && self.patches.is_empty()
            && self.symbol_match() != SymbolMatch::Mismatch
    }

    /// Replace the radio entry points in the assembled image.
    ///
    /// A separate step from booting, and it says what it did, because
    /// patched firmware is not running what it would run on hardware. Doing
    /// it silently as part of "run" would make that invisible.
    pub fn patch_radio(&mut self) -> Result<usize, SessionError> {
        let Some(flash_path) = self.flash_path.clone() else {
            return Ok(0);
        };
        // Symbols when the firmware shipped them, byte signatures when it did
        // not. Mismatched symbols are refused earlier rather than quietly
        // falling back here: a stale .elf gives confident, wrong addresses,
        // and silently ignoring it would hide that the pair is broken.
        let usable = self.symbols.as_ref().filter(|_| self.symbol_match() != SymbolMatch::Mismatch);

        let mut flash = std::fs::read(&flash_path)?;
        let mut patches = flashimg::patch::radio_bypass();
        let patcher = match usable {
            Some(symbols) => {
                flashimg::patch::Patcher::new(&symbols.table, &flash, APP_OFFSET)?
            }
            None => {
                // Not every entry point has a signature worth trusting; the
                // ones that do not are left alone rather than guessed at.
                patches.retain(|p| flashimg::signatures::has(&p.symbol));
                flashimg::patch::Patcher::from_signatures(&flash, APP_OFFSET)?
            }
        };
        let outcome = patcher.apply(&mut flash, &patches)?;
        let applied = outcome.applied;
        std::fs::write(&flash_path, &flash)?;

        let n = applied.len();
        self.patches = applied;
        Ok(n)
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
        self.open_serial_log();

        // Devices first: the emulator connects on startup and does not retry,
        // so the server has to be listening before QEMU exists.
        let hardware = hardware::start(&self.board, self.sd_image.clone())?;

        let qemu = Qemu::locate(chip)?;
        let mut config = LaunchConfig::new(chip, &flash);
        config.psram = self.board.qemu_psram();
        config.vpb_port = Some(hardware.port);
        config.display_dc_gpio = self.board.display_dc_gpio();
        config.serial_count = SerialBuffer::PORTS;

        // Extra QEMU flags, whitespace separated. Exists so a boot can be
        // traced without leaving the window -- `-d unimp -D some\file.log`
        // being the useful pair. Measuring headless meant measuring a
        // different device set than the one being used, which cost a day.
        if let Some(extra) = std::env::var_os("ESP32_QEMU_ARGS") {
            config.extra_args.extend(
                extra.to_string_lossy().split_whitespace().map(str::to_owned),
            );
        }

        self.instance = Some(Instance::spawn(&qemu, &config)?);
        self.hardware = Some(hardware);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut inst) = self.instance.take() {
            inst.shutdown();
        }
        // After the emulator, so the last transactions are still answered
        // while it shuts down rather than blocking on a server that is gone.
        self.hardware = None;
    }

    /// Pull any new serial output into the buffer. Call once per frame.
    pub fn pump(&mut self) {
        if let Some(inst) = &mut self.instance {
            for chunk in inst.read_serial() {
                self.serial.push(chunk.port, &chunk.bytes);
                // Tee to disk when asked. Measuring a boot used to mean
                // running headless, which is a different device set and a
                // different bus server -- so the thing being measured was
                // never the thing being used. This makes the window itself
                // the measurable configuration.
                if let Some(file) = &mut self.serial_log {
                    use std::io::Write;
                    let _ = file.write_all(&chunk.bytes);
                    let _ = file.flush();
                }
            }
        }
    }

    /// Type into a serial port, as a terminal would.
    pub fn send_serial(&mut self, port: usize, text: &str) {
        if let Some(inst) = &mut self.instance {
            let _ = inst.write_serial(port, text.as_bytes());
        }
    }

    /// Names for the machine's serial ports, in the order it wires them.
    pub fn serial_port_names(&self) -> &'static [&'static str] {
        // The S3 wires these unconditionally, so the console lands on the
        // third whether or not the first two are used.
        &["UART0", "UART1", "USB Serial/JTAG"]
    }
}

/// Every serial port's scrollback, plus a merged view of all of them.
///
/// Both are needed, and neither is sufficient. The ESP32-S3 ROM writes its
/// banner to UART0 *and* the USB console, so a merged view shows the whole
/// boot twice; but the application afterwards splits its output between them
/// -- PURR OS logs through IDF's console on one and its own logger on the
/// other -- so a single port shows half the story.
///
/// Per-port is the default because it is readable. The merged view is there
/// for the question per-port views cannot answer: what happened first.
#[derive(Debug)]
pub struct SerialLog {
    ports: [SerialBuffer; SerialBuffer::PORTS],
    merged: SerialBuffer,
}

impl Default for SerialLog {
    fn default() -> Self {
        SerialLog {
            ports: std::array::from_fn(|_| SerialBuffer::default()),
            merged: SerialBuffer {
                label_sources: true,
                ..SerialBuffer::default()
            },
        }
    }
}

impl SerialLog {
    pub fn push(&mut self, port: usize, bytes: &[u8]) {
        if let Some(buf) = self.ports.get_mut(port) {
            buf.push(port, bytes);
        }
        self.merged.push(port, bytes);
    }

    /// One port's output, or the merged view when `port` is out of range.
    pub fn view(&self, port: Option<usize>) -> &SerialBuffer {
        port.and_then(|p| self.ports.get(p)).unwrap_or(&self.merged)
    }

    /// Bytes seen on each port, so the UI can show which are alive.
    pub fn counts(&self) -> [usize; SerialBuffer::PORTS] {
        std::array::from_fn(|i| self.ports[i].per_port[i])
    }

    pub fn clear(&mut self) {
        for buf in &mut self.ports {
            buf.clear();
        }
        self.merged.clear();
    }
}

#[derive(Debug, Default)]
pub struct SerialBuffer {
    text: String,
    /// Undecoded bytes, kept for the terminal's hex view. Bounded separately
    /// and much smaller, since it is only useful for recent traffic.
    raw: std::collections::VecDeque<u8>,
    /// Bytes discarded from the front, so the UI can say the log was trimmed.
    pub trimmed: usize,
    /// Which port wrote last, so a marker is only emitted on a change.
    last_port: Option<usize>,
    /// Whether to mark where the source changes. On for the merged view, off
    /// for a single port's, where every line has the same source anyway.
    label_sources: bool,
    /// Bytes seen per port, so the UI can show which ones are alive.
    pub per_port: [usize; Self::PORTS],
}

impl SerialBuffer {
    /// Firmware can produce output indefinitely; keep the most recent slice.
    const LIMIT: usize = 512 * 1024;
    const RAW_LIMIT: usize = 16 * 1024;
    pub const PORTS: usize = 3;

    pub fn push(&mut self, port: usize, bytes: &[u8]) {
        if let Some(count) = self.per_port.get_mut(port) {
            *count += bytes.len();
        }

        if self.label_sources && self.last_port != Some(port) {
            // On its own line, so a marker never lands mid-sentence.
            if !self.text.is_empty() && !self.text.ends_with('\n') {
                self.text.push('\n');
            }
            let name = match port {
                0 => "UART0",
                1 => "UART1",
                2 => "USB Serial/JTAG",
                n => return self.push_untagged(n, bytes),
            };
            self.text.push_str(&format!("--- {name} ---\n"));
            self.last_port = Some(port);
        }

        self.push_untagged(port, bytes);
    }

    fn push_untagged(&mut self, _port: usize, bytes: &[u8]) {
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
        self.last_port = None;
        self.per_port = [0; Self::PORTS];
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

    /// A session carrying a symbol table and an image, without touching disk.
    fn paired(recorded: Option<[u8; 32]>, elf: [u8; 32]) -> Session {
        let mut s = Session::new(Board::from_toml(include_str!("../../../boards/generic-esp32s3.toml")).unwrap());
        s.firmware = Some(LoadedFirmware {
            path: PathBuf::from("app.bin"),
            chip: flashimg::Chip::Esp32S3,
            kind: "merged flash image",
            project: None,
            version: None,
            idf_version: None,
            size: 0,
            elf_sha256: recorded,
        });
        s.symbols = Some(LoadedSymbols {
            path: PathBuf::from("app.elf"),
            count: 1,
            sha256: elf,
            table: flashimg::ElfSymbols::default(),
        });
        s
    }

    #[test]
    fn symbols_from_another_build_are_refused() {
        // The real case this comes from: two builds of the same project, one
        // image loaded and the other's .elf left over from an earlier drop.
        // The addresses are plausible and completely wrong.
        let s = paired(Some([0xaa; 32]), [0xbb; 32]);
        assert_eq!(s.symbol_match(), SymbolMatch::Mismatch);
        assert!(!s.can_patch(), "patching a mismatched pair corrupts the image");
    }

    #[test]
    fn symbols_matching_the_image_are_accepted() {
        let s = paired(Some([0xaa; 32]), [0xaa; 32]);
        assert_eq!(s.symbol_match(), SymbolMatch::Matches);
    }

    #[test]
    fn an_image_without_a_digest_cannot_be_checked_either_way() {
        // Refusing here would break every build that leaves the field zeroed,
        // so this stays allowed -- flagged in the UI, not blocked.
        let s = paired(None, [0xbb; 32]);
        assert_eq!(s.symbol_match(), SymbolMatch::Unverifiable);
    }

    #[test]
    fn nothing_loaded_is_not_a_mismatch() {
        let s = Session::new(Board::from_toml(include_str!("../../../boards/generic-esp32s3.toml")).unwrap());
        assert_eq!(s.symbol_match(), SymbolMatch::Nothing);
    }

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
    fn an_elf_is_symbols_not_something_to_boot() {
        // The bootloader wants a flash image, so an ELF is never what we run
        // -- but it is the only place the symbol table lives, and patching
        // locates functions by symbol. Classifying it as firmware made
        // dropping one an error instead of the useful thing it is.
        assert_eq!(
            classify(Path::new("f.elf"), b"\x7fELF\x02\x01"),
            DropKind::Symbols
        );
        assert_eq!(
            classify(Path::new("no-extension"), b"\x7fELF\x02\x01"),
            DropKind::Symbols
        );
    }

    #[test]
    fn serial_buffer_strips_ansi_colour() {
        let mut b = SerialBuffer::default();
        b.push(0, b"\x1b[0;32mI (123) boot: ok\x1b[0m\n");
        assert!(b.text().ends_with("I (123) boot: ok\n"), "got {:?}", b.text());
        assert!(!b.text().contains('\x1b'));
    }

    #[test]
    fn the_merged_view_labels_where_the_source_changes() {
        // The case this exists for: a run that looks hung is often just output
        // going to a port nobody is reading.
        let mut log = SerialLog::default();
        log.push(2, b"ESP-ROM:esp32s3\n");
        log.push(0, b"I (200) app: hello\n");
        log.push(2, b"more rom\n");

        let text = log.view(None).text();
        assert!(text.starts_with("--- USB Serial/JTAG ---\n"));
        assert!(text.contains("--- UART0 ---\nI (200) app: hello\n"));
        // Switching back re-labels, so the order is never ambiguous.
        assert_eq!(text.matches("--- USB Serial/JTAG ---").count(), 2);
    }

    #[test]
    fn a_single_port_view_carries_only_that_port_and_no_labels() {
        // Labels would be noise here: every line has the same source. This is
        // the readable view, and the reason both exist -- the S3 ROM writes
        // its banner to UART0 *and* the USB console, so the merged view shows
        // the whole boot twice.
        let mut log = SerialLog::default();
        log.push(0, b"ESP-ROM\n");
        log.push(2, b"ESP-ROM\n");
        log.push(0, b"I (200) app: hello\n");

        let uart0 = log.view(Some(0)).text();
        assert_eq!(uart0, "ESP-ROM\nI (200) app: hello\n");
        assert_eq!(log.view(Some(2)).text(), "ESP-ROM\n");
        assert_eq!(log.view(Some(1)).text(), "", "nothing was sent to UART1");
    }

    #[test]
    fn byte_counts_say_which_ports_are_alive() {
        let mut log = SerialLog::default();
        log.push(0, b"hello");
        log.push(2, b"hi");
        assert_eq!(log.counts(), [5, 0, 2]);
    }

    #[test]
    fn serial_buffer_survives_invalid_utf8() {
        let mut b = SerialBuffer::default();
        b.push(0, &[0xff, 0xfe, b'o', b'k']);
        assert!(b.text().ends_with("ok"), "got {:?}", b.text());
    }

    #[test]
    fn serial_buffer_trims_without_splitting_characters() {
        let mut b = SerialBuffer::default();
        // Multi-byte characters straddling the cut point must not panic.
        for _ in 0..40_000 {
            b.push(0, "ünïcødé line\n".as_bytes());
        }
        assert!(b.trimmed > 0, "buffer should have been trimmed");
        assert!(b.text().len() <= SerialBuffer::LIMIT);
    }
}
